use super::*;
use crate::native::Registration;
use amaterasu_abi::{AbiHeader, BorrowedBytes, NativeInterface, NativeInterfaceQuery};
use std::{
    ffi::c_void,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use crate::worker::{Worker, WorkerBackend};

#[derive(Default)]
struct SlowGateState {
    first_submit_entered: bool,
    release_first_submit: bool,
    submits_started: usize,
    submits_completed: usize,
    packets: VecDeque<Packet>,
    drain_entered: bool,
    release_drain: bool,
    drain_completed: bool,
}

#[derive(Default)]
struct SlowGate {
    state: Mutex<SlowGateState>,
    changed: Condvar,
}

impl SlowGate {
    fn wait_for(&self, timeout: Duration, predicate: impl Fn(&SlowGateState) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock().unwrap();
        while !predicate(&state) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, result) = self.changed.wait_timeout(state, remaining).unwrap();
            state = next;
            if result.timed_out() && !predicate(&state) {
                return false;
            }
        }
        true
    }

    fn release_first_submit(&self) {
        self.state.lock().unwrap().release_first_submit = true;
        self.changed.notify_all();
    }

    fn release_drain(&self) {
        self.state.lock().unwrap().release_drain = true;
        self.changed.notify_all();
    }
}

struct ReleaseSlowGateOnDrop(Arc<SlowGate>);

impl Drop for ReleaseSlowGateOnDrop {
    fn drop(&mut self) {
        self.0.release_first_submit();
        self.0.release_drain();
    }
}

struct SlowWorkerBackend(Arc<SlowGate>);

impl WorkerBackend for SlowWorkerBackend {
    fn uses_background_encode(&self) -> bool {
        true
    }

    fn submit(&mut self, _: Vec<u8>, timestamp: i64, _: bool) -> Result<(), &'static str> {
        let mut state = self.0.state.lock().unwrap();
        state.submits_started += 1;
        if state.submits_started == 1 {
            state.first_submit_entered = true;
            self.0.changed.notify_all();
            while !state.release_first_submit {
                state = self.0.changed.wait(state).unwrap();
            }
        }
        state.packets.push_back(Packet {
            bytes: vec![0, 0, 1, 0x41],
            timestamp_100ns: timestamp,
            keyframe: false,
        });
        state.submits_completed += 1;
        self.0.changed.notify_all();
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<Packet>, &'static str> {
        Ok(self.0.state.lock().unwrap().packets.pop_front())
    }

    fn drain(&mut self) -> Result<(), &'static str> {
        let mut state = self.0.state.lock().unwrap();
        state.drain_entered = true;
        self.0.changed.notify_all();
        while !state.release_drain {
            state = self.0.changed.wait(state).unwrap();
        }
        state.drain_completed = true;
        self.0.changed.notify_all();
        Ok(())
    }
}
struct Fake {
    packets: VecDeque<Packet>,
    submissions: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

impl Backend for Fake {
    fn submit(&mut self, _: Vec<u8>, _: i64, _: bool) -> Result<SubmitResult, &'static str> {
        self.submissions.fetch_add(1, Ordering::SeqCst);
        Ok(SubmitResult::Accepted)
    }
    fn receive(&mut self) -> Result<ReceiveResult, &'static str> {
        Ok(ReceiveResult::Ready(self.packets.pop_front()))
    }
    fn drain(&mut self) -> Result<bool, &'static str> {
        Ok(true)
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

struct BusyOnceFake {
    busy: bool,
}

struct DeferredReceiveFake {
    pending_once: bool,
}

impl Backend for DeferredReceiveFake {
    fn submit(&mut self, _: Vec<u8>, _: i64, _: bool) -> Result<SubmitResult, &'static str> {
        Ok(SubmitResult::Accepted)
    }

    fn receive(&mut self) -> Result<ReceiveResult, &'static str> {
        if std::mem::replace(&mut self.pending_once, false) {
            Ok(ReceiveResult::Pending)
        } else {
            Ok(ReceiveResult::Ready(None))
        }
    }

    fn drain(&mut self) -> Result<bool, &'static str> {
        Ok(true)
    }
}

impl Backend for BusyOnceFake {
    fn submit(&mut self, _: Vec<u8>, _: i64, _: bool) -> Result<SubmitResult, &'static str> {
        if std::mem::replace(&mut self.busy, false) {
            Ok(SubmitResult::Busy)
        } else {
            Ok(SubmitResult::Accepted)
        }
    }

    fn receive(&mut self) -> Result<ReceiveResult, &'static str> {
        Ok(ReceiveResult::Ready(None))
    }

    fn drain(&mut self) -> Result<bool, &'static str> {
        Ok(true)
    }
}

impl Backend for StreamingFake {
    fn submit(
        &mut self,
        _: Vec<u8>,
        timestamp: i64,
        force: bool,
    ) -> Result<SubmitResult, &'static str> {
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
        Ok(SubmitResult::Accepted)
    }

    fn receive(&mut self) -> Result<ReceiveResult, &'static str> {
        Ok(ReceiveResult::Ready(self.packets.pop_front()))
    }

    fn drain(&mut self) -> Result<bool, &'static str> {
        Ok(true)
    }
}

fn configuration() -> Configuration {
    Configuration {
        width: 16,
        height: 16,
        fps: 60,
        bitrate: 1_000_000,
        color_mode: amaterasu_media::ColorMode::default(),
        max_reference_frames: 0,
        nvenc_preset: None,
        nvenc_h264_cavlc: false,
        nvenc_spatial_aq: false,
        amd_av1_latency_mode: crate::model::AmdAv1LatencyMode::Auto,
    }
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
fn asynchronous_encoder_backpressure_does_not_consume_frame_sequence_or_timestamp() {
    let mut sessions = Sessions::new(true);
    sessions
        .insert(
            "alice",
            configuration(),
            Box::new(BusyOnceFake { busy: true }),
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
        timestamp_us: 100,
        sequence: 7,
    };
    // SAFETY: frame storage remains live and immutable for both submit attempts.
    assert_eq!(
        unsafe { sessions.submit("alice", 1, &frame, 0) },
        MEDIA_BUSY
    );
    let session = sessions.values.get(&1).unwrap();
    assert!(session.metadata.is_empty());
    assert!(session.last_input.is_none());
    assert_eq!(unsafe { sessions.submit("alice", 1, &frame, 0) }, STATUS_OK);
    let session = sessions.values.get(&1).unwrap();
    assert_eq!(session.metadata.len(), 1);
    assert_eq!(session.last_input, Some((1000, 7)));
}

#[test]
fn asynchronous_drain_waits_for_encoder_poll_before_emitting_eos() {
    let mut sessions = Sessions::new(true);
    sessions
        .insert(
            "alice",
            configuration(),
            Box::new(DeferredReceiveFake { pending_once: true }),
            false,
        )
        .unwrap();
    sessions.values.get_mut(&1).unwrap().draining = true;
    let mut sink = Sink {
        reject: false,
        flags: Vec::new(),
        sequences: Vec::new(),
    };
    let packet_sink = PacketSink {
        context: (&mut sink as *mut Sink).cast(),
        write,
    };
    // SAFETY: packet sink context remains live through both receive callbacks.
    assert_eq!(
        unsafe { sessions.receive("alice", 1, packet_sink) },
        MEDIA_PENDING
    );
    assert!(sink.flags.is_empty());
    assert_eq!(
        unsafe { sessions.receive("alice", 1, packet_sink) },
        STATUS_OK
    );
    assert_eq!(sink.flags, [FLAG_END_OF_STREAM]);
}

#[test]
fn slow_worker_bounds_real_queue_preserves_retry_metadata_and_orders_drain_eos() {
    let gate = Arc::new(SlowGate::default());
    let worker = Worker::with_backend(SlowWorkerBackend(gate.clone()));
    let mut sessions = Sessions::new(true);
    sessions
        .insert("alice", configuration(), Box::new(worker), false)
        .unwrap();
    let _release_gate_on_exit = ReleaseSlowGateOnDrop(gate.clone());
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

    // SAFETY: frame points into `data`, which remains live and immutable until
    // all synchronous submit callbacks in this test finish.
    // The first submit is held inside the fake encoder while the bounded queue fills.
    assert_eq!(unsafe { sessions.submit("alice", 1, &frame, 0) }, STATUS_OK);
    let first_submit_entered =
        gate.wait_for(Duration::from_secs(2), |state| state.first_submit_entered);
    if !first_submit_entered {
        gate.release_first_submit();
    }
    assert!(first_submit_entered);
    for sequence in 2..=3 {
        frame.timestamp_us = sequence;
        frame.sequence = sequence;
        assert_eq!(unsafe { sessions.submit("alice", 1, &frame, 0) }, STATUS_OK);
    }
    frame.timestamp_us = 4;
    frame.sequence = 4;
    assert_eq!(
        unsafe { sessions.submit("alice", 1, &frame, 0) },
        MEDIA_BUSY
    );
    assert_eq!(
        unsafe { sessions.submit("alice", 1, &frame, 0) },
        MEDIA_BUSY
    );
    {
        let session = sessions.values.get(&1).unwrap();
        assert_eq!(session.metadata.len(), 3);
        assert_eq!(session.last_input, Some((30, 3)));
    }
    assert_eq!(sessions.drain("alice", 1), MEDIA_BUSY);

    gate.release_first_submit();
    assert!(gate.wait_for(Duration::from_secs(2), |state| state.submits_completed == 3));
    let retry_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match unsafe { sessions.submit("alice", 1, &frame, 0) } {
            STATUS_OK => break,
            MEDIA_BUSY if Instant::now() < retry_deadline => std::thread::yield_now(),
            status => panic!("frame retry should be accepted, got {status}"),
        }
    }
    assert!(gate.wait_for(Duration::from_secs(2), |state| state.submits_completed == 4));
    let drain_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match sessions.drain("alice", 1) {
            STATUS_OK => break,
            MEDIA_BUSY if Instant::now() < drain_deadline => std::thread::yield_now(),
            status => panic!("drain should be admitted, got {status}"),
        }
    }
    let drain_entered = gate.wait_for(Duration::from_secs(2), |state| state.drain_entered);
    if !drain_entered {
        gate.release_drain();
    }
    assert!(drain_entered);

    let mut sink = Sink {
        reject: false,
        flags: Vec::new(),
        sequences: Vec::new(),
    };
    let packet_sink = PacketSink {
        context: (&mut sink as *mut Sink).cast(),
        write,
    };
    // A pending worker drain cannot be mistaken for an empty, completed stream.
    // SAFETY: packet sink context stays live throughout the receive loop.
    assert_eq!(
        unsafe { sessions.receive("alice", 1, packet_sink) },
        MEDIA_PENDING
    );
    assert!(sink.flags.is_empty());
    gate.release_drain();
    assert!(gate.wait_for(Duration::from_secs(2), |state| state.drain_completed));

    let deadline = Instant::now() + Duration::from_secs(2);
    // SAFETY: `packet_sink` continues to reference the live `sink` until loop exit.
    while sink.flags.last() != Some(&FLAG_END_OF_STREAM) && Instant::now() < deadline {
        let status = unsafe { sessions.receive("alice", 1, packet_sink) };
        assert!(matches!(status, STATUS_OK | MEDIA_PENDING));
        if status == MEDIA_PENDING {
            std::thread::yield_now();
        }
    }
    assert_eq!(sink.sequences, [1, 2, 3, 4]);
    assert_eq!(
        sink.flags
            .iter()
            .filter(|flag| **flag == FLAG_END_OF_STREAM)
            .count(),
        1
    );
}

#[test]
fn nondefault_color_rejects_nv12_without_submitting_ambiguous_pixels() {
    let submissions = Arc::new(AtomicUsize::new(0));
    let mut sessions = Sessions::new(true);
    let mut configuration = configuration();
    configuration.color_mode = amaterasu_media::ColorMode::Bt601Limited;
    sessions
        .insert(
            "alice",
            configuration,
            Box::new(Fake {
                packets: VecDeque::new(),
                submissions: submissions.clone(),
                drops: Arc::new(AtomicUsize::new(0)),
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

    // SAFETY: owned data remains live and immutable for the submit call.
    assert_eq!(
        unsafe { sessions.submit("alice", 1, &frame, 0) },
        STATUS_INVALID_ARGUMENT
    );
    assert_eq!(submissions.load(Ordering::SeqCst), 0);
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
