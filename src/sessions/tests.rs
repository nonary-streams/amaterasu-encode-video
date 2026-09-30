use super::*;
use crate::native::Registration;
use amaterasu_abi::{AbiHeader, BorrowedBytes, NativeInterface, NativeInterfaceQuery};
use std::{
    ffi::c_void,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
struct Fake {
    packets: VecDeque<Packet>,
    submissions: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

impl Backend for Fake {
    fn submit(&mut self, _: Vec<u8>, _: i64, _: bool) -> Result<(), &'static str> {
        self.submissions.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn receive(&mut self) -> Result<Option<Packet>, &'static str> {
        Ok(self.packets.pop_front())
    }
    fn drain(&mut self) -> Result<(), &'static str> {
        Ok(())
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

struct StreamingFake {
    packets: VecDeque<Packet>,
    submissions: Arc<AtomicUsize>,
    honor_force: bool,
}

impl Backend for StreamingFake {
    fn submit(&mut self, _: Vec<u8>, timestamp: i64, force: bool) -> Result<(), &'static str> {
        self.submissions.fetch_add(1, Ordering::SeqCst);
        self.packets.push_back(Packet {
            bytes: vec![
                0,
                0,
                1,
                if force && self.honor_force {
                    0x65
                } else {
                    0x41
                },
            ],
            timestamp_100ns: timestamp,
            keyframe: force && self.honor_force,
        });
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<Packet>, &'static str> {
        Ok(self.packets.pop_front())
    }

    fn drain(&mut self) -> Result<(), &'static str> {
        Ok(())
    }
}

fn configuration() -> Configuration {
    Configuration {
        width: 16,
        height: 16,
        fps: 60,
        bitrate: 1_000_000,
        min_threads: 2,
        max_reference_frames: 0,
        nvenc_preset: None,
        nvenc_h264_cavlc: false,
        nvenc_spatial_aq: false,
        amd_av1_latency_mode: crate::model::AmdAv1LatencyMode::Auto,
        sw_preset: None,
        sw_tune: None,
    }
}

#[cfg(amaterasu_x264)]
#[test]
#[ignore = "requires matching system x264 runtime"]
fn software_native_contract_preserves_busy_input_retry_ownership_and_eos() {
    struct SoftwareSink {
        reject_data_once: bool,
        reject_eos_once: bool,
        attempted: Option<(Vec<u8>, u64, u64, u32)>,
        accepted: Vec<(Vec<u8>, u64, u64, u32)>,
    }
    unsafe extern "C" fn collect(context: *mut c_void, packet: *const EncodedPacket) -> Status {
        // SAFETY: test supplies live callback state and borrowed packet storage.
        let sink = unsafe { &mut *context.cast::<SoftwareSink>() };
        let packet = unsafe { &*packet };
        let bytes = if packet.len == 0 {
            Vec::new()
        } else {
            // SAFETY: callback-lifetime bytes are owned by the encoder session.
            unsafe { std::slice::from_raw_parts(packet.data, packet.len) }.to_vec()
        };
        let value = (bytes, packet.timestamp_us, packet.sequence, packet.flags);
        if packet.flags & FLAG_END_OF_STREAM != 0 && sink.reject_eos_once {
            sink.reject_eos_once = false;
            return MEDIA_BUSY;
        }
        if sink.reject_data_once {
            sink.reject_data_once = false;
            sink.attempted = Some(value);
            return MEDIA_BUSY;
        }
        if let Some(attempted) = sink.attempted.take() {
            assert_eq!(value, attempted, "refused packet must be retried exactly");
        }
        sink.accepted.push(value);
        STATUS_OK
    }
    let mut sessions = Sessions::new(true);
    let config = configuration();
    let identity = crate::platform::enumerate()
        .unwrap()
        .into_iter()
        .find(|encoder| !encoder.hardware)
        .expect("software backend available")
        .identity;
    sessions
        .open(
            "alice",
            identity.clone(),
            config.clone(),
            Some("software-test".into()),
            true,
        )
        .unwrap();
    let mut changed = config.clone();
    changed.sw_tune = Some("film".into());
    assert_eq!(
        sessions.open(
            "alice",
            identity,
            changed,
            Some("software-test".into()),
            true
        ),
        Err("operation_conflict")
    );
    let mut sink = SoftwareSink {
        reject_data_once: true,
        reject_eos_once: true,
        attempted: None,
        accepted: Vec::new(),
    };
    let callback = PacketSink {
        context: (&mut sink as *mut SoftwareSink).cast(),
        write: collect,
    };
    let bytes = vec![128; 20 * 16 * 3 / 2];
    for sequence in 0..16 {
        let frame = CpuFrame {
            width: 16,
            height: 16,
            format: PIXEL_NV12,
            stride: 20,
            data: bytes.as_ptr(),
            len: bytes.len(),
            timestamp_us: (sequence + 1) * 16667,
            sequence,
        };
        // SAFETY: test retains all declared frame storage during submission.
        assert_eq!(
            unsafe { sessions.submit("bob", 1, &frame, 0) },
            MEDIA_CLOSED
        );
        assert_eq!(
            unsafe { sessions.submit("alice", 1, &frame, u32::from(sequence == 7)) },
            STATUS_OK
        );
    }
    let frame = CpuFrame {
        width: 16,
        height: 16,
        format: PIXEL_NV12,
        stride: 20,
        data: bytes.as_ptr(),
        len: bytes.len(),
        timestamp_us: 17 * 16667,
        sequence: 16,
    };
    // SAFETY: frame allocation and callback state remain live for this exercise.
    unsafe {
        assert_eq!(sessions.submit("alice", 1, &frame, 0), MEDIA_BUSY);
        assert_eq!(sessions.receive("bob", 1, callback), MEDIA_CLOSED);
        assert_eq!(sessions.receive("alice", 1, callback), MEDIA_BUSY);
        while sessions.receive("alice", 1, callback) == STATUS_OK {}
        assert!((1..=16).contains(&sink.accepted.len()));
        // The busy submission accepted nothing; the identical input is valid now.
        assert_eq!(sessions.submit("alice", 1, &frame, 0), STATUS_OK);
        assert_eq!(sessions.drain("bob", 1), MEDIA_CLOSED);
        assert_eq!(sessions.drain("alice", 1), STATUS_OK);
        assert_eq!(sessions.submit("alice", 1, &frame, 0), MEDIA_CLOSED);
        // Frame threads may retain bounded delayed output even for zerolatency.
        // Drain must deliver all of it before the deliberately refused EOS.
        loop {
            match sessions.receive("alice", 1, callback) {
                STATUS_OK => {}
                MEDIA_BUSY => break,
                status => panic!("unexpected status before refused EOS: {status}"),
            }
        }
        assert_eq!(sessions.receive("alice", 1, callback), STATUS_OK);
        assert_eq!(sessions.receive("alice", 1, callback), MEDIA_PENDING);
    }
    assert_eq!(sink.accepted.len(), 18);
    for (index, (bytes, timestamp, sequence, flags)) in sink.accepted[..17].iter().enumerate() {
        assert_eq!(*sequence, index as u64);
        assert_eq!(*timestamp, (index as u64 + 1) * 16667);
        assert!(bytes.starts_with(&[0, 0, 0, 1]));
        assert_ne!(flags & FLAG_END_OF_ACCESS_UNIT, 0);
        assert_eq!(flags & FLAG_KEYFRAME != 0, index == 0 || index == 7);
    }
    assert_eq!(sink.accepted[17], (Vec::new(), 0, 0, FLAG_END_OF_STREAM));
    sessions.close("alice", 1).unwrap();
    assert!(matches!(
        sessions.lookup("alice", "software-test"),
        Err("session_not_found")
    ));
}

fn encoder_identity() -> EncoderIdentity {
    EncoderIdentity {
        clsid: "test-clsid".into(),
        hardware_url: Some("test://hardware-mft".into()),
        friendly_name: "Test H.264".into(),
    }
}

struct Sink {
    reject: bool,
    flags: Vec<u32>,
    sequences: Vec<u64>,
}

unsafe extern "C" fn write(context: *mut c_void, packet: *const EncodedPacket) -> Status {
    // SAFETY: test supplies a live Sink and receives a callback-lifetime packet.
    let sink = unsafe { &mut *context.cast::<Sink>() };
    let packet = unsafe { &*packet };
    if sink.reject {
        return MEDIA_BUSY;
    }
    sink.flags.push(packet.flags);
    sink.sequences.push(packet.sequence);
    STATUS_OK
}

unsafe fn binding(registration: &Registration, owner: &str) -> *const EncoderApi {
    let query = NativeInterfaceQuery {
        header: AbiHeader::of::<NativeInterfaceQuery>(),
        interface_major: 1,
        min_minor: 0,
        min_size: size_of::<EncoderApi>() as u32,
    };
    let mut output: *const NativeInterface = std::ptr::null();
    // SAFETY: local valid factory/query/output and principal storage.
    assert_eq!(
        unsafe {
            registration.factory.bind.unwrap()(
                registration.factory.context,
                BorrowedBytes::new(owner.as_bytes()),
                &query,
                &mut output,
            )
        },
        STATUS_OK
    );
    output.cast()
}

#[test]
fn native_bindings_enforce_owner_preserve_rejected_packets_and_emit_eos_once() {
    exercise_packet_flags(false);
}

#[test]
fn boundary_opt_in_marks_data_and_never_combines_eos() {
    exercise_packet_flags(true);
}

fn exercise_packet_flags(packet_boundaries: bool) {
    let submissions = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let mut sessions = Sessions::new(true);
    sessions
        .insert(
            "alice",
            configuration(),
            Box::new(Fake {
                packets: VecDeque::from([Packet {
                    bytes: vec![0, 0, 1, 0x41, 1],
                    timestamp_100ns: 10,
                    keyframe: false,
                }]),
                submissions: submissions.clone(),
                drops: drops.clone(),
            }),
            packet_boundaries,
        )
        .unwrap();
    let shared = Arc::new(Mutex::new(sessions));
    let registration = Registration::new(shared.clone());
    let mut sink = Sink {
        reject: true,
        flags: Vec::new(),
        sequences: Vec::new(),
    };
    let packet_sink = PacketSink {
        context: (&mut sink as *mut Sink).cast(),
        write,
    };
    let data = vec![0; 16 * 16 * 4];
    let frame = CpuFrame {
        width: 16,
        height: 16,
        format: PIXEL_BGRA8,
        stride: 64,
        data: data.as_ptr(),
        len: data.len(),
        timestamp_us: 1,
        sequence: 42,
    };
    // SAFETY: test retains factories, bindings, frame allocations and callback
    // context through all calls, then releases each binding exactly once.
    unsafe {
        let alice = &*binding(&registration, "alice");
        let bob = &*binding(&registration, "bob");
        assert_eq!((bob.submit)(bob.base.context, 1, &frame, 0), MEDIA_CLOSED);
        assert_eq!(
            (bob.receive)(bob.base.context, 1, packet_sink),
            MEDIA_CLOSED
        );
        assert_eq!((bob.drain)(bob.base.context, 1), MEDIA_CLOSED);
        assert_eq!((alice.submit)(alice.base.context, 1, &frame, 0), STATUS_OK);
        assert_eq!(submissions.load(Ordering::SeqCst), 1);
        assert_eq!(
            (alice.receive)(alice.base.context, 1, packet_sink),
            MEDIA_BUSY
        );
        sink.reject = false;
        assert_eq!(
            (alice.receive)(alice.base.context, 1, packet_sink),
            STATUS_OK
        );
        assert_eq!(sink.sequences, [42]);
        assert_eq!((alice.drain)(alice.base.context, 1), STATUS_OK);
        assert_eq!(
            (alice.submit)(alice.base.context, 1, &frame, 0),
            MEDIA_CLOSED
        );
        assert_eq!(
            (alice.receive)(alice.base.context, 1, packet_sink),
            STATUS_OK
        );
        assert_eq!(
            (alice.receive)(alice.base.context, 1, packet_sink),
            MEDIA_PENDING
        );
        let data_flags = if packet_boundaries {
            FLAG_END_OF_ACCESS_UNIT
        } else {
            0
        };
        assert_eq!(sink.flags, [data_flags, FLAG_END_OF_STREAM]);
        assert!(sink.flags.iter().all(|flags| flags
            & (FLAG_END_OF_STREAM | FLAG_END_OF_ACCESS_UNIT)
            != (FLAG_END_OF_STREAM | FLAG_END_OF_ACCESS_UNIT)));
        shared.lock().unwrap().close("alice", 1).unwrap();
        assert_eq!(
            (alice.receive)(alice.base.context, 1, packet_sink),
            MEDIA_CLOSED
        );
        alice.base.release.unwrap()(alice.base.context);
        bob.base.release.unwrap()(bob.base.context);
    }
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn bounded_pending_input_returns_busy_without_accepting() {
    let submissions = Arc::new(AtomicUsize::new(0));
    let mut sessions = Sessions::new(true);
    sessions
        .insert(
            "alice",
            configuration(),
            Box::new(Fake {
                packets: VecDeque::new(),
                submissions: submissions.clone(),
                drops: Arc::new(AtomicUsize::new(0)),
            }),
            false,
        )
        .unwrap();
    let data = vec![0; 384];
    let mut frame = CpuFrame {
        width: 16,
        height: 16,
        format: PIXEL_NV12,
        stride: 16,
        data: data.as_ptr(),
        len: data.len(),
        timestamp_us: 0,
        sequence: 0,
    };
    for sequence in 0..16 {
        frame.timestamp_us = sequence;
        frame.sequence = sequence;
        // SAFETY: owned data remains live and immutable.
        assert_eq!(unsafe { sessions.submit("alice", 1, &frame, 0) }, STATUS_OK);
    }
    frame.timestamp_us = 16;
    frame.sequence = 16;
    assert_eq!(
        unsafe { sessions.submit("alice", 1, &frame, 0) },
        MEDIA_BUSY
    );
    assert_eq!(submissions.load(Ordering::SeqCst), 16);
}

#[test]
fn delivered_packet_releases_metadata_capacity_without_losing_input_order() {
    let submissions = Arc::new(AtomicUsize::new(0));
    let mut sessions = Sessions::new(true);
    sessions
        .insert(
            "alice",
            configuration(),
            Box::new(StreamingFake {
                packets: VecDeque::new(),
                submissions: submissions.clone(),
                honor_force: true,
            }),
            false,
        )
        .unwrap();
    let data = vec![0; 384];
    let mut frame = CpuFrame {
        width: 16,
        height: 16,
        format: PIXEL_NV12,
        stride: 16,
        data: data.as_ptr(),
        len: data.len(),
        timestamp_us: 1,
        sequence: 1,
    };
    for sequence in 1..=16 {
        frame.timestamp_us = sequence;
        frame.sequence = sequence;
        // SAFETY: the borrowed frame allocation remains live for every call.
        assert_eq!(
            unsafe { sessions.submit("alice", 1, &frame, u32::from(sequence == 1)) },
            STATUS_OK
        );
    }
    frame.timestamp_us = 17;
    frame.sequence = 17;
    assert_eq!(
        unsafe { sessions.submit("alice", 1, &frame, 0) },
        MEDIA_BUSY
    );
    assert_eq!(submissions.load(Ordering::SeqCst), 16);

    let mut sink = Sink {
        reject: true,
        flags: Vec::new(),
        sequences: Vec::new(),
    };
    let packet_sink = PacketSink {
        context: (&mut sink as *mut Sink).cast(),
        write,
    };
    assert_eq!(
        unsafe { sessions.receive("bob", 1, packet_sink) },
        MEDIA_CLOSED
    );
    assert_eq!(
        unsafe { sessions.receive("alice", 1, packet_sink) },
        MEDIA_BUSY
    );
    assert!(sink.sequences.is_empty());
    assert_eq!(sessions.values.get(&1).unwrap().metadata.len(), 16);
    sink.reject = false;
    assert_eq!(
        unsafe { sessions.receive("alice", 1, packet_sink) },
        STATUS_OK
    );
    assert_eq!(sink.sequences, [1]);
    assert_eq!(sink.flags, [FLAG_KEYFRAME]);
    assert_eq!(sessions.values.get(&1).unwrap().metadata.len(), 15);

    assert_eq!(unsafe { sessions.submit("alice", 1, &frame, 0) }, STATUS_OK);
    assert_eq!(submissions.load(Ordering::SeqCst), 17);
    assert_eq!(
        unsafe { sessions.receive("alice", 1, packet_sink) },
        STATUS_OK
    );
    assert_eq!(sink.sequences, [1, 2]);
    frame.timestamp_us = 1;
    frame.sequence = 1;
    assert_eq!(
        unsafe { sessions.submit("alice", 1, &frame, 0) },
        STATUS_INVALID_ARGUMENT
    );
    assert_eq!(submissions.load(Ordering::SeqCst), 17);

    assert_eq!(sessions.drain("alice", 1), STATUS_OK);
    for _ in 3..=17 {
        assert_eq!(
            unsafe { sessions.receive("alice", 1, packet_sink) },
            STATUS_OK
        );
    }
    assert_eq!(sink.sequences, (1..=17).collect::<Vec<_>>());
    assert_eq!(sessions.values.get(&1).unwrap().metadata.len(), 0);
    assert_eq!(
        unsafe { sessions.receive("alice", 1, packet_sink) },
        STATUS_OK
    );
    assert_eq!(sink.flags.last(), Some(&FLAG_END_OF_STREAM));
    assert_eq!(
        unsafe { sessions.receive("alice", 1, packet_sink) },
        MEDIA_PENDING
    );
    sessions.close("alice", 1).unwrap();
    assert_eq!(
        unsafe { sessions.receive("alice", 1, packet_sink) },
        MEDIA_CLOSED
    );
}

#[test]
fn forced_frame_without_keyframe_fails_before_delivery_and_closes_cleanly() {
    let mut sessions = Sessions::new(true);
    sessions
        .insert(
            "alice",
            configuration(),
            Box::new(StreamingFake {
                packets: VecDeque::new(),
                submissions: Arc::new(AtomicUsize::new(0)),
                honor_force: false,
            }),
            false,
        )
        .unwrap();
    let data = vec![0; 384];
    let frame = CpuFrame {
        width: 16,
        height: 16,
        format: PIXEL_NV12,
        stride: 16,
        data: data.as_ptr(),
        len: data.len(),
        timestamp_us: 1,
        sequence: 1,
    };
    let mut sink = Sink {
        reject: false,
        flags: Vec::new(),
        sequences: Vec::new(),
    };
    let packet_sink = PacketSink {
        context: (&mut sink as *mut Sink).cast(),
        write,
    };
    assert_eq!(unsafe { sessions.submit("alice", 1, &frame, 1) }, STATUS_OK);
    assert_eq!(
        unsafe { sessions.receive("alice", 1, packet_sink) },
        MEDIA_BACKEND_ERROR
    );
    assert!(sink.sequences.is_empty());
    assert_eq!(sessions.status("alice", 1).unwrap()["state"], "failed");
    assert_eq!(sessions.drain("alice", 1), MEDIA_BACKEND_ERROR);
    sessions.close("alice", 1).unwrap();
    assert_eq!(
        unsafe { sessions.receive("alice", 1, packet_sink) },
        MEDIA_CLOSED
    );
}

#[test]
fn control_validates_limits_disabled_default_and_operation_tombstones() {
    let mut sessions = Sessions::new(false);
    assert_eq!(
        sessions
            .open("alice", encoder_identity(), configuration(), None, false)
            .unwrap_err(),
        "disabled"
    );
    let mut config = configuration();
    config.width = 17;
    assert!(config.validate().is_err());
    assert!(
        serde_json::from_value::<crate::model::Operation>(
        json!({"action":"open","encoder":encoder_identity(),"configuration":configuration(),"owner":"bob"})
        )
        .is_err()
    );
    sessions.operations.insert(
        ("alice".into(), "one".into()),
        OpenRecord {
            packet_boundaries: false,
            id: 1,
            encoder: encoder_identity(),
            configuration: configuration(),
        },
    );
    assert_eq!(
        sessions
            .open(
                "alice",
                encoder_identity(),
                configuration(),
                Some("one".into()),
                false
            )
            .unwrap_err(),
        "session_closed"
    );
    assert_eq!(
        sessions.lookup("bob", "one").unwrap_err(),
        "session_not_found"
    );
}

#[test]
fn nvenc_preset_is_rejected_at_the_session_boundary_before_opening() {
    let mut sessions = Sessions::new(true);
    let mut config = configuration();
    config.nvenc_preset = Some(crate::model::NvencPreset::P1);

    assert_eq!(
        sessions
            .open("alice", encoder_identity(), config, None, false)
            .unwrap_err(),
        "nvenc_preset_unsupported"
    );
    assert_eq!(sessions.latest_id(), 0);
    assert!(sessions.values.is_empty());
}

#[test]
fn nvenc_h264_cavlc_is_rejected_before_worker_creation_and_operation_lookup() {
    let mut sessions = Sessions::new(true);
    let mut config = configuration();
    config.nvenc_h264_cavlc = true;

    assert_eq!(
        sessions
            .open(
                "alice",
                encoder_identity(),
                config.clone(),
                Some("cavlc".into()),
                false,
            )
            .unwrap_err(),
        "nvenc_h264_cavlc_unsupported"
    );
    assert_eq!(sessions.latest_id(), 0);
    assert!(sessions.values.is_empty());
    assert!(sessions.operations.is_empty());

    sessions.operations.insert(
        ("alice".into(), "existing".into()),
        OpenRecord {
            packet_boundaries: false,
            id: 1,
            encoder: encoder_identity(),
            configuration: configuration(),
        },
    );
    assert_eq!(
        sessions
            .open(
                "alice",
                encoder_identity(),
                config,
                Some("existing".into()),
                false,
            )
            .unwrap_err(),
        "nvenc_h264_cavlc_unsupported"
    );
    assert_eq!(sessions.latest_id(), 0);
    assert!(sessions.values.is_empty());
}

#[test]
fn nvenc_spatial_aq_is_rejected_before_worker_creation_and_operation_lookup() {
    let mut sessions = Sessions::new(true);
    let mut config = configuration();
    config.nvenc_spatial_aq = true;

    assert_eq!(
        sessions
            .open(
                "alice",
                encoder_identity(),
                config.clone(),
                Some("spatial-aq".into()),
                false,
            )
            .unwrap_err(),
        "nvenc_spatial_aq_unsupported"
    );
    assert_eq!(sessions.latest_id(), 0);
    assert!(sessions.values.is_empty());
    assert!(sessions.operations.is_empty());

    sessions.operations.insert(
        ("alice".into(), "existing".into()),
        OpenRecord {
            packet_boundaries: false,
            id: 1,
            encoder: encoder_identity(),
            configuration: configuration(),
        },
    );
    assert_eq!(
        sessions
            .open(
                "alice",
                encoder_identity(),
                config,
                Some("existing".into()),
                false,
            )
            .unwrap_err(),
        "nvenc_spatial_aq_unsupported"
    );
    assert_eq!(sessions.latest_id(), 0);
    assert!(sessions.values.is_empty());
}

#[test]
fn disabled_nvenc_spatial_aq_preserves_mft_configuration_validation() {
    let sessions = Sessions::new(true);
    let config = configuration();

    assert!(!config.nvenc_spatial_aq);
    assert!(config.validate().is_ok());
    assert!(
        sessions
            .validate_open("alice", &encoder_identity(), &config)
            .is_ok()
    );
}

#[test]
fn amd_av1_latency_mode_is_rejected_at_the_session_boundary_before_opening() {
    let mut sessions = Sessions::new(true);
    let mut config = configuration();
    config.amd_av1_latency_mode = crate::model::AmdAv1LatencyMode::Realtime;

    assert_eq!(
        sessions
            .open("alice", encoder_identity(), config.clone(), None, false)
            .unwrap_err(),
        "amd_av1_latency_mode_unsupported"
    );
    assert_eq!(sessions.latest_id(), 0);
    assert!(sessions.values.is_empty());
    assert!(sessions.operations.is_empty());

    sessions.operations.insert(
        ("alice".into(), "existing".into()),
        OpenRecord {
            packet_boundaries: false,
            id: 1,
            encoder: encoder_identity(),
            configuration: configuration(),
        },
    );
    assert_eq!(
        sessions
            .open(
                "alice",
                encoder_identity(),
                config,
                Some("existing".into()),
                false,
            )
            .unwrap_err(),
        "amd_av1_latency_mode_unsupported"
    );
    assert_eq!(sessions.latest_id(), 0);
    assert!(sessions.values.is_empty());
}

#[test]
fn changing_boundary_mode_conflicts_with_existing_operation() {
    let mut sessions = Sessions::new(true);
    sessions.operations.insert(
        ("alice".into(), "one".into()),
        OpenRecord {
            packet_boundaries: false,
            id: 1,
            encoder: encoder_identity(),
            configuration: configuration(),
        },
    );
    assert_eq!(
        sessions
            .open(
                "alice",
                encoder_identity(),
                configuration(),
                Some("one".into()),
                true
            )
            .unwrap_err(),
        "operation_conflict"
    );
    let operation: crate::model::Operation = serde_json::from_value(
        json!({"action":"open","encoder":encoder_identity(),"configuration":configuration()}),
    )
    .unwrap();
    assert!(matches!(
        operation,
        crate::model::Operation::Open {
            packet_boundaries: false,
            ..
        }
    ));
}

#[test]
fn reference_frame_limit_defaults_validates_and_participates_in_idempotency() {
    let operation: crate::model::Operation = serde_json::from_value(json!({
        "action":"open",
        "encoder":encoder_identity(),
        "configuration":{"width":16,"height":16,"fps":60,"bitrate":1_000_000}
    }))
    .unwrap();
    let crate::model::Operation::Open {
        configuration: legacy,
        ..
    } = operation
    else {
        panic!("expected open operation");
    };
    assert_eq!(legacy.max_reference_frames, 0);
    assert!(
        serde_json::to_value(&legacy).unwrap()["max_reference_frames"].is_null(),
        "legacy configuration must retain its old wire shape"
    );

    let mut strict = legacy.clone();
    strict.max_reference_frames = 1;
    assert_eq!(
        serde_json::to_value(&strict).unwrap()["max_reference_frames"],
        1
    );

    let mut invalid = configuration();
    invalid.max_reference_frames = 2;
    assert_eq!(invalid.validate(), Err("invalid_configuration"));

    let mut sessions = Sessions::new(true);
    sessions.operations.insert(
        ("alice".into(), "reference-limit".into()),
        OpenRecord {
            packet_boundaries: false,
            id: 1,
            encoder: encoder_identity(),
            configuration: configuration(),
        },
    );
    let mut changed = configuration();
    changed.max_reference_frames = 1;
    assert_eq!(open_reply(7, &changed, false)["max_reference_frames"], 1);
    assert_eq!(
        sessions
            .open(
                "alice",
                encoder_identity(),
                changed,
                Some("reference-limit".into()),
                false,
            )
            .unwrap_err(),
        "operation_conflict"
    );
}
