//! Session behaviour for GPU-converted inputs: packed BGRA and DMA-BUF
//! frames. Pipe descriptors stand in for DMA-BUFs and sync_files; writing to
//! a pipe fails with EPIPE once every read descriptor is closed, which shows
//! whether a duplicate is still open without racing on descriptor numbers.
use super::*;
use crate::input::{DmaBufImportSupport, GpuInput, InputSupport};
use crate::native::Registration;
use crate::worker::{ReceiveResult, SubmitResult, Worker, WorkerBackend};
use amaterasu_abi::{AbiHeader, BorrowedBytes, NativeInterface, NativeInterfaceQuery};
use std::{
    io::{PipeWriter, Write},
    os::fd::{AsRawFd, OwnedFd},
    sync::{Arc, Mutex},
};

const XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
const LINEAR: u64 = 0;

#[derive(Default)]
struct Observed {
    nv12: Vec<Vec<u8>>,
    bgra: Vec<Vec<u8>>,
    /// Accepted DMA-BUF inputs stay alive here until a test drops them.
    dmabuf: Vec<crate::input::DmaBufInput>,
    cuda: Vec<crate::input::CudaInput>,
}

struct GpuFake {
    support: InputSupport,
    capacity: Arc<Mutex<bool>>,
    observed: Arc<Mutex<Observed>>,
    completion: bool,
}

impl Backend for GpuFake {
    fn submit(&mut self, nv12: Vec<u8>, _: i64, _: bool) -> Result<SubmitResult, &'static str> {
        self.observed.lock().unwrap().nv12.push(nv12);
        Ok(SubmitResult::Accepted)
    }
    fn receive(&mut self) -> Result<ReceiveResult, &'static str> {
        Ok(ReceiveResult::Ready(None))
    }
    fn drain(&mut self) -> Result<bool, &'static str> {
        Ok(true)
    }
    fn input_support(&self) -> InputSupport {
        self.support.clone()
    }
    fn has_gpu_submit_capacity(&mut self) -> Result<bool, &'static str> {
        Ok(*self.capacity.lock().unwrap())
    }
    fn supports_gpu_completion(&self) -> bool {
        self.completion
    }
    fn submit_gpu(
        &mut self,
        input: GpuInput,
        _: i64,
        _: bool,
    ) -> Result<SubmitResult, &'static str> {
        let mut observed = self.observed.lock().unwrap();
        match input {
            GpuInput::Bgra(bytes) => observed.bgra.push(bytes),
            GpuInput::DmaBuf(frame) => observed.dmabuf.push(frame),
            GpuInput::Cuda(frame) => observed.cuda.push(frame),
        }
        Ok(SubmitResult::Accepted)
    }
}

struct Harness {
    sessions: Sessions,
    id: u64,
    capacity: Arc<Mutex<bool>>,
    observed: Arc<Mutex<Observed>>,
}

fn import_support() -> Arc<DmaBufImportSupport> {
    let mut support = DmaBufImportSupport::default();
    support.add_format(XRGB8888, true, [LINEAR]);
    Arc::new(support)
}

fn harness(support: InputSupport) -> Harness {
    let capacity = Arc::new(Mutex::new(true));
    let observed = Arc::new(Mutex::new(Observed::default()));
    let mut sessions = Sessions::new(true);
    let backend = GpuFake {
        support,
        capacity: capacity.clone(),
        observed: observed.clone(),
        completion: false,
    };
    sessions
        .insert("alice", configuration(), Box::new(backend), false)
        .unwrap();
    Harness {
        id: sessions.latest_id(),
        sessions,
        capacity,
        observed,
    }
}

fn gpu_harness() -> Harness {
    harness(InputSupport {
        gpu_bgra_conversion: true,
        gpu_scaling: false,
        dmabuf: Some(import_support()),
        cuda_device: None,
    })
}

fn configuration() -> Configuration {
    serde_json::from_value(serde_json::json!({
        "width": 16, "height": 16, "fps": 60, "bitrate": 1_000_000
    }))
    .unwrap()
}

fn stand_in() -> (OwnedFd, PipeWriter) {
    let (reader, writer) = std::io::pipe().unwrap();
    (reader.into(), writer)
}

fn reader_closed(writer: &mut PipeWriter) -> bool {
    writer.write(&[0]).is_err()
}

fn frame(plane: &OwnedFd, fence: Option<&OwnedFd>, sequence: u64) -> DmaBufFrame {
    let mut planes = [DmaBufPlane::UNUSED; DMABUF_MAX_PLANES];
    planes[0] = DmaBufPlane {
        fd: plane.as_raw_fd(),
        acquire_fence_fd: fence.map_or(-1, |fence| fence.as_raw_fd()),
        offset: 0,
        pitch: 64,
    };
    DmaBufFrame {
        width: 16,
        height: 16,
        fourcc: XRGB8888,
        plane_count: 1,
        modifier: LINEAR,
        planes,
        color: DMABUF_COLOR_SRGB,
        reserved: 0,
        timestamp_us: sequence * 1_000,
        sequence,
    }
}

fn submit(harness: &mut Harness, frame: &DmaBufFrame, force: u32) -> Status {
    // SAFETY: every descriptor in the test frames is open for the call.
    unsafe {
        harness
            .sessions
            .submit_dmabuf("alice", harness.id, frame, force)
    }
}

fn close_on_exec(descriptor: i32) -> bool {
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{descriptor}")).unwrap();
    let flags = info
        .lines()
        .find_map(|line| line.strip_prefix("flags:"))
        .map(|value| u32::from_str_radix(value.trim(), 8).unwrap())
        .unwrap();
    flags & 0o2_000_000 != 0
}

#[test]
fn accepted_dmabuf_frames_own_close_on_exec_duplicates_until_dropped() {
    let mut harness = gpu_harness();
    let (plane, mut plane_writer) = stand_in();
    let (fence, mut fence_writer) = stand_in();
    let borrowed = frame(&plane, Some(&fence), 1);
    assert_eq!(submit(&mut harness, &borrowed, 1), STATUS_OK);

    // The producer may close its descriptors as soon as submit returns.
    let (plane_number, fence_number) = (plane.as_raw_fd(), fence.as_raw_fd());
    drop((plane, fence));
    assert!(!reader_closed(&mut plane_writer));
    assert!(!reader_closed(&mut fence_writer));
    {
        let observed = harness.observed.lock().unwrap();
        let accepted = &observed.dmabuf[0];
        assert_eq!((accepted.width, accepted.height), (16, 16));
        assert_eq!((accepted.fourcc, accepted.modifier), (XRGB8888, LINEAR));
        let plane = &accepted.planes[0];
        assert_eq!((plane.offset, plane.pitch), (0, 64));
        assert_ne!(plane.fd.as_raw_fd(), plane_number);
        let duplicate_fence = plane.acquire_fence.as_ref().unwrap().as_raw_fd();
        assert_ne!(duplicate_fence, fence_number);
        assert!(close_on_exec(plane.fd.as_raw_fd()));
        assert!(close_on_exec(duplicate_fence));
    }
    harness.observed.lock().unwrap().dmabuf.clear();
    assert!(reader_closed(&mut plane_writer));
    assert!(reader_closed(&mut fence_writer));

    let session = harness.sessions.values.get(&harness.id).unwrap();
    assert_eq!(session.metadata.len(), 1);
    assert!(session.metadata[0].force_keyframe);
    assert_eq!(session.last_input, Some((10_000, 1)));
}

#[test]
fn busy_and_unsupported_submissions_duplicate_nothing() {
    let mut harness = gpu_harness();
    let (plane, mut plane_writer) = stand_in();

    *harness.capacity.lock().unwrap() = false;
    assert_eq!(submit(&mut harness, &frame(&plane, None, 1), 0), MEDIA_BUSY);
    *harness.capacity.lock().unwrap() = true;

    let mut hdr = frame(&plane, None, 1);
    hdr.color = DMABUF_COLOR_BT2020_PQ;
    assert_eq!(submit(&mut harness, &hdr, 0), MEDIA_UNSUPPORTED);
    let mut tiled = frame(&plane, None, 1);
    tiled.modifier = 0x0300_0000_0000_0014;
    assert_eq!(submit(&mut harness, &tiled, 0), MEDIA_UNSUPPORTED);
    let mut yuv = frame(&plane, None, 1);
    yuv.fourcc = u32::from_le_bytes(*b"NV12");
    assert_eq!(submit(&mut harness, &yuv, 0), MEDIA_UNSUPPORTED);

    assert!(harness.observed.lock().unwrap().dmabuf.is_empty());
    let session = harness.sessions.values.get(&harness.id).unwrap();
    assert!(session.metadata.is_empty() && session.last_input.is_none());
    drop(plane);
    assert!(reader_closed(&mut plane_writer), "a duplicate was retained");

    // Implicit modifiers are admitted when the backend listed the format.
    let (plane, _writer) = stand_in();
    let mut implicit = frame(&plane, None, 2);
    implicit.modifier = DRM_FORMAT_MOD_INVALID;
    assert_eq!(submit(&mut harness, &implicit, 0), STATUS_OK);
}

#[test]
fn only_scaling_backends_accept_independent_gpu_source_dimensions() {
    for scaling in [false, true] {
        let mut host = harness(InputSupport {
            gpu_bgra_conversion: true,
            gpu_scaling: scaling,
            dmabuf: Some(import_support()),
            cuda_device: None,
        });
        let (plane, mut writer) = stand_in();
        let mut source = frame(&plane, None, 1);
        source.width = 32;
        source.height = 48;
        source.planes[0].pitch = source.width * 4;
        assert_eq!(
            submit(&mut host, &source, 1),
            if scaling {
                STATUS_OK
            } else {
                MEDIA_UNSUPPORTED
            }
        );
        let observed = host.observed.lock().unwrap();
        assert_eq!(observed.dmabuf.len(), usize::from(scaling));
        if scaling {
            assert_eq!(
                (observed.dmabuf[0].width, observed.dmabuf[0].height),
                (32, 48)
            );
        }
        drop(observed);
        let session = host.sessions.values.get(&host.id).unwrap();
        assert_eq!((session.config.width, session.config.height), (16, 16));
        assert_eq!(session.last_input.is_some(), scaling);
        host.observed.lock().unwrap().dmabuf.clear();
        drop(plane);
        assert!(reader_closed(&mut writer));
    }
}

#[test]
fn backends_without_gpu_import_report_unsupported() {
    let mut harness = harness(InputSupport::default());
    let (plane, mut plane_writer) = stand_in();
    assert_eq!(
        submit(&mut harness, &frame(&plane, None, 1), 0),
        MEDIA_UNSUPPORTED
    );
    drop(plane);
    assert!(reader_closed(&mut plane_writer));
}

#[test]
fn malformed_dmabuf_frames_are_rejected_before_duplication() {
    let mut harness = gpu_harness();
    let (plane, mut plane_writer) = stand_in();
    let valid = frame(&plane, None, 5);

    let mut no_planes = valid;
    no_planes.plane_count = 0;
    let mut negative_fd = valid;
    negative_fd.planes[0].fd = -1;
    let mut reserved = valid;
    reserved.reserved = 1;
    let mut stray_plane = valid;
    stray_plane.planes[1].fd = plane.as_raw_fd();
    let mut force_two = valid;
    force_two.sequence = 6;
    for (label, candidate, force) in [
        ("no planes", no_planes, 0),
        ("negative fd", negative_fd, 0),
        ("reserved", reserved, 0),
        ("unused plane", stray_plane, 0),
        ("force", force_two, 2),
    ] {
        assert_eq!(
            submit(&mut harness, &candidate, force),
            STATUS_INVALID_ARGUMENT,
            "{label}"
        );
    }
    let mut not_open = valid;
    not_open.planes[0].fd = 2_000_000_000;
    assert_eq!(submit(&mut harness, &not_open, 0), STATUS_INVALID_ARGUMENT);

    assert_eq!(submit(&mut harness, &valid, 0), STATUS_OK);
    let mut repeated = valid;
    repeated.timestamp_us += 1;
    assert_eq!(submit(&mut harness, &repeated, 0), STATUS_INVALID_ARGUMENT);
    harness.observed.lock().unwrap().dmabuf.clear();
    drop(plane);
    assert!(reader_closed(&mut plane_writer));
}

#[test]
fn draining_failed_and_foreign_sessions_keep_their_status_codes() {
    let mut harness = gpu_harness();
    let (plane, _writer) = stand_in();
    let borrowed = frame(&plane, None, 1);
    // SAFETY: the descriptor is open for the call.
    let foreign = unsafe {
        harness
            .sessions
            .submit_dmabuf("mallory", harness.id, &borrowed, 0)
    };
    assert_eq!(foreign, MEDIA_CLOSED);
    assert_eq!(harness.sessions.drain("alice", harness.id), STATUS_OK);
    assert_eq!(submit(&mut harness, &borrowed, 0), MEDIA_CLOSED);
    harness.sessions.values.get_mut(&harness.id).unwrap().failed = true;
    assert_eq!(submit(&mut harness, &borrowed, 0), MEDIA_BACKEND_ERROR);
}

#[test]
fn bgra_frames_reach_gpu_backends_unconverted_and_nv12_keeps_its_path() {
    let mut harness = gpu_harness();
    let stride = 16 * 4 + 8;
    let mut bgra = vec![0xaa; stride * 16];
    for row in 0..16 {
        bgra[row * stride..row * stride + 64].fill(row as u8);
    }
    let cpu = CpuFrame {
        width: 16,
        height: 16,
        format: PIXEL_BGRA8,
        stride: stride as u32,
        data: bgra.as_ptr(),
        len: bgra.len(),
        timestamp_us: 1,
        sequence: 1,
    };
    // SAFETY: the frame borrows live bytes of the declared length.
    assert_eq!(
        unsafe { harness.sessions.submit("alice", harness.id, &cpu, 0) },
        STATUS_OK
    );
    let nv12 = vec![0x10; 16 * 24];
    let cpu_nv12 = CpuFrame {
        format: PIXEL_NV12,
        stride: 16,
        data: nv12.as_ptr(),
        len: nv12.len(),
        timestamp_us: 2,
        sequence: 2,
        ..cpu
    };
    // SAFETY: as above.
    assert_eq!(
        unsafe { harness.sessions.submit("alice", harness.id, &cpu_nv12, 0) },
        STATUS_OK
    );

    let observed = harness.observed.lock().unwrap();
    let packed = &observed.bgra[0];
    assert_eq!(packed.len(), 16 * 16 * 4);
    assert!((0..16).all(|row| {
        packed[row * 64..][..64]
            .iter()
            .all(|byte| *byte == row as u8)
    }));
    assert_eq!(observed.nv12, std::slice::from_ref(&nv12));
    let session = harness.sessions.values.get(&harness.id).unwrap();
    assert_eq!(session.color_conversion_frames, 0, "no CPU conversion ran");
}

#[test]
fn the_native_table_preserves_old_prefixes_and_offers_leased_submission() {
    let registration = Registration::new(Arc::new(Mutex::new(Sessions::new(true))));
    let bind = |min_minor: u32, min_size: u32| {
        let query = NativeInterfaceQuery {
            header: AbiHeader::of::<NativeInterfaceQuery>(),
            interface_major: 1,
            min_minor,
            min_size,
        };
        let mut output: *const NativeInterface = std::ptr::null();
        // SAFETY: local factory, query, principal and output storage.
        let status = unsafe {
            registration.factory.bind.unwrap()(
                registration.factory.context,
                BorrowedBytes::new(b"alice"),
                &query,
                &mut output,
            )
        };
        (status, output)
    };
    let (status, output) = bind(ENCODER_DMABUF_INTERFACE_MINOR, ENCODER_API_V1_1_SIZE);
    assert_eq!(status, STATUS_OK);
    // SAFETY: a successful bind returns a live v1.1 table until release.
    unsafe {
        let api = &*output.cast::<EncoderApiV1_1>();
        // The newest table is offered; every older prefix stays intact.
        assert_eq!(api.base.header.minor, ENCODER_RESOURCE_INTERFACE_MINOR);
        assert_eq!(api.base.header.size, ENCODER_API_V1_5_SIZE);
        let (plane, _writer) = stand_in();
        let borrowed = frame(&plane, None, 1);
        assert_eq!(
            (api.submit_dmabuf)(api.base.context, 99, &borrowed, 0),
            MEDIA_CLOSED
        );
        assert_eq!(
            (api.submit_dmabuf)(api.base.context, 99, std::ptr::null(), 0),
            STATUS_INVALID_ARGUMENT
        );
        api.base.release.unwrap()(api.base.context);
    }
    let (status, output) = bind(0, ENCODER_API_V1_0_SIZE);
    assert_eq!(status, STATUS_OK);
    // SAFETY: as above; a v1.0 consumer reads only the unchanged prefix.
    unsafe {
        let api = &*output.cast::<EncoderApi>();
        api.base.release.unwrap()(api.base.context);
    }
    let (status, output) = bind(ENCODER_LEASE_INTERFACE_MINOR, ENCODER_API_V1_2_SIZE);
    assert_eq!(status, STATUS_OK);
    // SAFETY: the successful v1.2 query returns the complete additive table.
    unsafe {
        let api = &*output.cast::<EncoderApiV1_2>();
        assert_eq!(api.base.header.size, ENCODER_API_V1_5_SIZE);
        assert_eq!(
            (api.submit_dmabuf_leased)(api.base.context, 99, std::ptr::null(), 0),
            STATUS_INVALID_ARGUMENT
        );
        api.base.release.unwrap()(api.base.context);
    }
    assert_eq!(
        std::mem::offset_of!(EncoderApiV1_2, submit_dmabuf_leased),
        ENCODER_API_V1_1_SIZE as usize
    );
    let (status, output) = bind(ENCODER_RESOURCE_INTERFACE_MINOR, ENCODER_API_V1_5_SIZE);
    assert_eq!(status, STATUS_OK);
    // SAFETY: the v1.5 query returns the complete table; unknown sessions are closed.
    unsafe {
        let api = &*output.cast::<EncoderApiV1_5>();
        let mut generation = 0;
        let sink = CaptureResourceOfferSink {
            context: std::ptr::null_mut(),
            write: collect_offers,
        };
        let context = api.base.base.base.base.context;
        assert_eq!(
            (api.enumerate_resources)(context, 99, sink, &mut generation),
            MEDIA_CLOSED
        );
        assert_eq!(
            (api.submit_resource)(context, 99, std::ptr::null(), 0),
            STATUS_INVALID_ARGUMENT
        );
        api.base.base.base.base.release.unwrap()(context);
    }
    assert_eq!(
        std::mem::offset_of!(EncoderApiV1_5, enumerate_resources),
        ENCODER_API_V1_4_SIZE as usize
    );
    assert_eq!(
        bind(ENCODER_RESOURCE_INTERFACE_MINOR + 1, ENCODER_API_V1_2_SIZE).0,
        MEDIA_UNSUPPORTED
    );
    assert_eq!(bind(0, ENCODER_API_V1_5_SIZE + 8).0, MEDIA_UNSUPPORTED);
}

#[test]
fn gpu_readiness_is_authorized_and_never_accepts_or_reserves_an_input() {
    let capacity = Arc::new(Mutex::new(false));
    let observed = Arc::new(Mutex::new(Observed::default()));
    let mut sessions = Sessions::new(true);
    sessions
        .insert(
            "alice",
            configuration(),
            Box::new(GpuFake {
                support: InputSupport {
                    gpu_bgra_conversion: true,
                    gpu_scaling: false,
                    dmabuf: Some(import_support()),
                    cuda_device: None,
                },
                capacity: capacity.clone(),
                observed: observed.clone(),
                completion: true,
            }),
            false,
        )
        .unwrap();
    let id = sessions.latest_id();
    assert_eq!(
        sessions.encode_timing("mallory", id).unwrap_err(),
        MEDIA_CLOSED
    );
    assert_eq!(
        sessions
            .encode_timing("alice", id)
            .unwrap()
            .completed_frames,
        0
    );
    assert_eq!(sessions.gpu_input_ready("mallory", id), MEDIA_CLOSED);
    assert_eq!(sessions.gpu_input_ready("alice", id), MEDIA_BUSY);
    *capacity.lock().unwrap() = true;
    assert_eq!(sessions.gpu_input_ready("alice", id), STATUS_OK);
    assert_eq!(sessions.gpu_input_ready("alice", id), STATUS_OK);
    assert!(sessions.values[&id].metadata.is_empty());
    assert!(observed.lock().unwrap().dmabuf.is_empty());
    sessions.values.get_mut(&id).unwrap().draining = true;
    assert_eq!(sessions.gpu_input_ready("alice", id), MEDIA_CLOSED);
}

#[test]
fn installed_compatibility_table_keeps_the_timing_prefix_and_exposes_readiness() {
    let capacity = Arc::new(Mutex::new(true));
    let mut sessions = Sessions::new(true);
    sessions
        .insert(
            "alice",
            configuration(),
            Box::new(GpuFake {
                support: InputSupport {
                    gpu_bgra_conversion: true,
                    gpu_scaling: false,
                    dmabuf: Some(import_support()),
                    cuda_device: None,
                },
                capacity,
                observed: Arc::new(Mutex::new(Observed::default())),
                completion: true,
            }),
            false,
        )
        .unwrap();
    let id = sessions.latest_id();
    let registration = Registration::new(Arc::new(Mutex::new(sessions)));
    let query = NativeInterfaceQuery {
        header: AbiHeader::of::<NativeInterfaceQuery>(),
        interface_major: 1,
        min_minor: ENCODER_GPU_READINESS_INTERFACE_MINOR,
        min_size: ENCODER_API_V1_4_SIZE,
    };
    let mut output: *const NativeInterface = std::ptr::null();
    // SAFETY: all factory/query data and the returned table outlive these calls.
    unsafe {
        assert_eq!(
            registration.factory.bind.unwrap()(
                registration.factory.context,
                BorrowedBytes::new(b"alice"),
                &query,
                &mut output,
            ),
            STATUS_OK
        );
        let api = &*output.cast::<EncoderApiV1_4>();
        let context = api.base.base.base.context;
        let mut timing = EncodeTimingSnapshotV1::empty(11);
        assert_eq!(
            (api.base.encode_timing)(context, id, &mut timing),
            STATUS_OK
        );
        assert_eq!(timing, EncodeTimingSnapshotV1::empty(1));
        assert_eq!((api.gpu_input_ready)(context, id), STATUS_OK);
        assert_eq!((api.gpu_input_ready)(context, id + 1), MEDIA_CLOSED);
        api.base.base.base.release.unwrap()(context);
    }
}

/// Holds every GPU input it receives, blocking the first until released.
struct HoldingBackend {
    gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
    entered: Arc<(Mutex<bool>, std::sync::Condvar)>,
    held: Arc<Mutex<Vec<GpuInput>>>,
}

impl WorkerBackend for HoldingBackend {
    fn uses_background_encode(&self) -> bool {
        true
    }
    fn submit(&mut self, _: Vec<u8>, _: i64, _: bool) -> Result<(), &'static str> {
        Ok(())
    }
    fn receive(&mut self) -> Result<Option<Packet>, &'static str> {
        Ok(None)
    }
    fn drain(&mut self) -> Result<(), &'static str> {
        Ok(())
    }
    fn input_support(&self) -> InputSupport {
        InputSupport {
            gpu_bgra_conversion: true,
            gpu_scaling: false,
            dmabuf: Some(import_support()),
            cuda_device: None,
        }
    }
    fn submit_gpu(&mut self, input: GpuInput, _: i64, _: bool) -> Result<(), &'static str> {
        *self.entered.0.lock().unwrap() = true;
        self.entered.1.notify_all();
        let (released, changed) = &*self.gate;
        let mut released = released.lock().unwrap();
        while !*released {
            released = changed.wait(released).unwrap();
        }
        self.held.lock().unwrap().push(input);
        Ok(())
    }
}

#[test]
fn worker_capacity_guarantees_acceptance_and_close_drops_queued_frames() {
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let held = Arc::new(Mutex::new(Vec::new()));
    let entered = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let mut worker = Worker::with_backend(HoldingBackend {
        gate: gate.clone(),
        entered: entered.clone(),
        held: held.clone(),
    });
    assert!(worker.input_support().dmabuf.is_some());
    let mut writers = Vec::new();
    let mut accepted = 0;
    while worker.has_gpu_submit_capacity().unwrap() {
        let (plane, writer) = stand_in();
        writers.push(writer);
        let frame = frame(&plane, None, accepted + 1);
        // SAFETY: the descriptor is open for the call.
        let input = unsafe { crate::input::DmaBufInput::duplicate(&frame) }.unwrap();
        assert_eq!(
            worker.submit_gpu(GpuInput::DmaBuf(input), accepted as i64, false),
            Ok(SubmitResult::Accepted)
        );
        accepted += 1;
        assert!(accepted <= 1, "only one GPU frame may wait for encoding");
    }
    assert!(accepted >= 1);
    // Establish that one input actually reached the backend before cancelling;
    // otherwise a fast test thread can cancel the entire queue before it starts.
    let (started, _) = entered
        .1
        .wait_timeout_while(
            entered.0.lock().unwrap(),
            std::time::Duration::from_secs(5),
            |started| !*started,
        )
        .unwrap();
    assert!(
        *started,
        "the worker did not start its first GPU submission"
    );
    drop(started);
    // Close while the first input is still inside the backend: queued
    // inputs are discarded with the channel, closing their duplicates.
    let closer = std::thread::spawn(move || drop(worker));
    std::thread::sleep(std::time::Duration::from_millis(50));
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    closer.join().unwrap();
    // The running submission completes; cancellation skips queued ones
    // unless the worker reached them before close began.
    let completed = held.lock().unwrap().len();
    assert!((1..=accepted as usize).contains(&completed));
    held.lock().unwrap().clear();
    for writer in &mut writers {
        assert!(reader_closed(writer));
    }
}

#[derive(Default)]
struct LeaseCallbacks {
    retained: std::sync::atomic::AtomicUsize,
    released: std::sync::atomic::AtomicUsize,
}

unsafe extern "C" fn retain_test_lease(context: *mut std::ffi::c_void) -> Status {
    // SAFETY: tests keep this callback state live throughout the borrowed call.
    let callbacks = unsafe { &*context.cast::<LeaseCallbacks>() };
    callbacks
        .retained
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    STATUS_OK
}

unsafe extern "C" fn release_test_lease(context: *mut std::ffi::c_void, fence: i32) {
    // SAFETY: tests keep this callback state live throughout the borrowed call.
    let callbacks = unsafe { &*context.cast::<LeaseCallbacks>() };
    assert_eq!(fence, -1);
    callbacks
        .released
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

fn leased_frame(frame: DmaBufFrame, callbacks: &LeaseCallbacks) -> DmaBufFrameLease {
    DmaBufFrameLease {
        frame,
        context: std::ptr::from_ref(callbacks).cast_mut().cast(),
        retain: Some(retain_test_lease),
        release: Some(release_test_lease),
    }
}

fn assert_no_retained_lease(callbacks: &LeaseCallbacks) {
    use std::sync::atomic::Ordering;
    assert_eq!(callbacks.retained.load(Ordering::Relaxed), 0);
    assert_eq!(callbacks.released.load(Ordering::Relaxed), 0);
}

#[test]
fn rejected_leased_submissions_consume_neither_references_nor_descriptors() {
    let mut harness = gpu_harness();
    let (plane, mut writer) = stand_in();
    let callbacks = LeaseCallbacks::default();
    let mut lease = leased_frame(frame(&plane, None, 1), &callbacks);
    *harness.capacity.lock().unwrap() = false;
    // SAFETY: the frame descriptors and callback state remain live until return.
    assert_eq!(
        unsafe {
            harness
                .sessions
                .submit_dmabuf_leased("alice", harness.id, &lease, 0)
        },
        MEDIA_BUSY
    );
    *harness.capacity.lock().unwrap() = true;
    // A backend that only duplicates descriptors cannot satisfy leased input
    // completion, even when its format support otherwise matches this frame.
    // SAFETY: the valid lease and descriptor stay live for the rejected call.
    assert_eq!(
        unsafe {
            harness
                .sessions
                .submit_dmabuf_leased("alice", harness.id, &lease, 0)
        },
        MEDIA_UNSUPPORTED
    );
    lease.frame.color = DMABUF_COLOR_BT2020_PQ;
    // SAFETY: as above; unsupported color is rejected before any worker call.
    assert_eq!(
        unsafe {
            harness
                .sessions
                .submit_dmabuf_leased("alice", harness.id, &lease, 0)
        },
        MEDIA_UNSUPPORTED
    );
    lease.frame.color = DMABUF_COLOR_SRGB;
    // SAFETY: as above; another owner's session is not accessible.
    assert_eq!(
        unsafe {
            harness
                .sessions
                .submit_dmabuf_leased("mallory", harness.id, &lease, 0)
        },
        MEDIA_CLOSED
    );
    lease.retain = None;
    // SAFETY: descriptor and state are live; the malformed callback table is rejected.
    assert_eq!(
        unsafe {
            harness
                .sessions
                .submit_dmabuf_leased("alice", harness.id, &lease, 0)
        },
        STATUS_INVALID_ARGUMENT
    );
    assert_no_retained_lease(&callbacks);
    assert!(harness.observed.lock().unwrap().dmabuf.is_empty());
    drop(plane);
    assert!(
        reader_closed(&mut writer),
        "a rejected frame retained a duplicate"
    );
}

struct CompletionBackend {
    entered: std::sync::mpsc::Sender<()>,
    gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
    fail: bool,
}

impl WorkerBackend for CompletionBackend {
    fn uses_background_encode(&self) -> bool {
        true
    }
    fn supports_gpu_completion(&self) -> bool {
        true
    }
    fn submit(&mut self, _: Vec<u8>, _: i64, _: bool) -> Result<(), &'static str> {
        Ok(())
    }
    fn receive(&mut self) -> Result<Option<Packet>, &'static str> {
        Ok(None)
    }
    fn drain(&mut self) -> Result<(), &'static str> {
        Ok(())
    }
    fn input_support(&self) -> InputSupport {
        InputSupport {
            gpu_bgra_conversion: true,
            gpu_scaling: false,
            dmabuf: Some(import_support()),
            cuda_device: None,
        }
    }
    fn submit_gpu(&mut self, input: GpuInput, _: i64, _: bool) -> Result<(), &'static str> {
        self.entered.send(()).unwrap();
        let mut released = self.gate.0.lock().unwrap();
        while !*released {
            released = self.gate.1.wait(released).unwrap();
        }
        // This fake models a completed GPU copy, so it releases source access
        // before reporting success or failure to the worker.
        drop(input);
        if self.fail {
            Err("controlled_gpu_copy_failure")
        } else {
            Ok(())
        }
    }
}

struct ReleaseCompletionGate(Arc<(Mutex<bool>, std::sync::Condvar)>);

impl Drop for ReleaseCompletionGate {
    fn drop(&mut self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }
}

#[test]
fn leased_submission_does_not_block_and_shutdown_retires_the_reference() {
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::time::Duration;
    for fail in [false, true] {
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let release_gate = ReleaseCompletionGate(gate.clone());
        let (entered, entry) = mpsc::channel();
        let (plane, mut writer) = stand_in();
        let worker = Worker::with_backend(CompletionBackend {
            entered,
            gate,
            fail,
        });
        let mut sessions = Sessions::new(true);
        sessions
            .insert("alice", configuration(), Box::new(worker), false)
            .unwrap();
        let id = sessions.latest_id();
        let callbacks = LeaseCallbacks::default();
        let lease = leased_frame(frame(&plane, None, 1), &callbacks);
        // SAFETY: callback state remains alive until close retires the reference.
        assert_eq!(
            unsafe { sessions.submit_dmabuf_leased("alice", id, &lease, 0) },
            STATUS_OK
        );
        entry.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(callbacks.retained.load(Ordering::Relaxed), 1);
        assert_eq!(callbacks.released.load(Ordering::Relaxed), 0);
        drop(plane);
        assert!(!reader_closed(&mut writer));
        drop(release_gate);
        sessions.close("alice", id).unwrap();
        assert_eq!(callbacks.released.load(Ordering::Relaxed), 1);
        assert!(reader_closed(&mut writer));
    }
}

#[test]
fn cooperative_shutdown_keeps_leases_until_the_native_worker_has_retired() {
    use amaterasu_module_sdk::ManagedModule;
    use std::sync::{atomic::Ordering, mpsc};
    use std::time::{Duration, Instant};

    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let (entered, entry) = mpsc::channel();
    let worker = Worker::with_backend(CompletionBackend {
        entered,
        gate: gate.clone(),
        fail: false,
    });
    let mut sessions = Sessions::new(true);
    sessions
        .insert("alice", configuration(), Box::new(worker), false)
        .unwrap();
    let id = sessions.latest_id();
    let (plane, mut writer) = stand_in();
    let callbacks = LeaseCallbacks::default();
    let lease = leased_frame(frame(&plane, None, 1), &callbacks);
    // SAFETY: callback state and immutable source storage remain live until
    // cooperative shutdown joins the worker and releases the retained lease.
    assert_eq!(
        unsafe { sessions.submit_dmabuf_leased("alice", id, &lease, 0) },
        STATUS_OK
    );
    let mut module = super::shutdown_tests::module(sessions);
    let release = ReleaseCompletionGate(gate);
    let entered = entry.recv_timeout(Duration::from_secs(3));
    drop(plane);
    let ready = module.quiesce();
    let released_while_running = callbacks.released.load(Ordering::Relaxed);
    let descriptors_retained = !reader_closed(&mut writer);
    drop(release);
    assert!(entered.is_ok());
    assert_eq!(ready, Ok(false));
    assert_eq!(released_while_running, 0);
    assert!(descriptors_retained);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !module.quiesce().unwrap() {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(callbacks.released.load(Ordering::Relaxed), 1);
    assert!(reader_closed(&mut writer));
}

#[test]
fn broker_close_waits_for_gpu_leases_and_observes_native_cancel_retirement() {
    use amaterasu_module_sdk::ManagedModule;
    use std::sync::{atomic::Ordering, mpsc};
    use std::time::Duration;

    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let (entered, entry) = mpsc::channel();
    let worker = Worker::with_backend(CompletionBackend {
        entered,
        gate: gate.clone(),
        fail: false,
    });
    let mut sessions = Sessions::new(true);
    sessions
        .insert("alice", configuration(), Box::new(worker), false)
        .unwrap();
    let id = sessions.latest_id();
    let (plane, mut writer) = stand_in();
    let callbacks = LeaseCallbacks::default();
    let lease = leased_frame(frame(&plane, None, 1), &callbacks);
    // SAFETY: callback state and the source plane outlive the retained reference.
    assert_eq!(
        unsafe { sessions.submit_dmabuf_leased("alice", id, &lease, 0) },
        STATUS_OK
    );
    let mut module = super::shutdown_tests::module(sessions);
    let release = ReleaseCompletionGate(gate);
    let broker = super::shutdown_tests::Broker::attach(&mut module);
    let entered = entry.recv_timeout(Duration::from_secs(3));
    broker.request(61, "alice", json!({"action":"close","session_id":id}));
    let first = module.tick();
    let completed_early = broker.completed();
    let released_early = callbacks.released.load(Ordering::Relaxed);
    drop(plane);
    let descriptors_retained = !reader_closed(&mut writer);
    broker.cancel(61);
    broker.request(73, "alice", json!({"action":"close","session_id":id}));
    let retried = module.tick();
    drop(release);
    assert!(entered.is_ok());
    assert!(first.is_ok());
    assert!(retried.is_ok());
    assert!(completed_early.is_empty());
    assert_eq!(released_early, 0);
    assert!(descriptors_retained);
    let binding = bind_lease_encoder(&module._registration, "alice");
    // SAFETY: this fixture owns the live binding and invokes cancellation on
    // the lease's owning thread. It remains a synchronous source-read barrier.
    unsafe {
        let api = &*binding;
        (api.cancel_dmabuf_leases)(api.base.context, id);
        api.base.release.unwrap()(api.base.context);
    }
    assert_eq!(callbacks.released.load(Ordering::Relaxed), 1);
    assert!(reader_closed(&mut writer));
    module.tick().unwrap();
    assert_eq!(
        broker.completed(),
        vec![(73, json!({"ok":true,"session_id":id,"state":"closed"}))]
    );
}

fn bind_lease_encoder(registration: &Registration, owner: &str) -> *const EncoderApiV1_2 {
    let query = NativeInterfaceQuery {
        header: AbiHeader::of::<NativeInterfaceQuery>(),
        interface_major: 1,
        min_minor: ENCODER_LEASE_INTERFACE_MINOR,
        min_size: ENCODER_API_V1_2_SIZE,
    };
    let mut output = std::ptr::null();
    // SAFETY: the factory, principal, query and output remain live for binding.
    let status = unsafe {
        registration.factory.bind.unwrap()(
            registration.factory.context,
            BorrowedBytes::new(owner.as_bytes()),
            &query,
            &mut output,
        )
    };
    assert_eq!(status, STATUS_OK);
    output.cast()
}

#[test]
fn native_lease_cancellation_is_owner_scoped_and_idempotent() {
    let harness = gpu_harness();
    let id = harness.id;
    let shared = Arc::new(Mutex::new(harness.sessions));
    let registration = Registration::new(shared.clone());
    let alice = bind_lease_encoder(&registration, "alice");
    let mallory = bind_lease_encoder(&registration, "mallory");
    // SAFETY: both native bindings and the backing sessions remain live.
    unsafe {
        let mallory = &*mallory;
        (mallory.cancel_dmabuf_leases)(mallory.base.context, id);
        assert!(shared.lock().unwrap().status("alice", id).is_ok());
        let alice = &*alice;
        (alice.cancel_dmabuf_leases)(alice.base.context, id);
        (alice.cancel_dmabuf_leases)(alice.base.context, id);
        assert!(shared.lock().unwrap().status("alice", id).is_err());
        alice.base.release.unwrap()(alice.base.context);
        mallory.base.release.unwrap()(mallory.base.context);
    }
}

#[test]
fn native_lease_cancellation_waits_for_running_gpu_work_and_releases_source() {
    use std::{
        sync::mpsc::{self, TryRecvError},
        time::Duration,
    };
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let release_gate = ReleaseCompletionGate(gate.clone());
    let (entered, entry) = mpsc::channel();
    let worker = Worker::with_backend(CompletionBackend {
        entered,
        gate,
        fail: false,
    });
    let mut sessions = Sessions::new(true);
    sessions
        .insert("alice", configuration(), Box::new(worker), false)
        .unwrap();
    let id = sessions.latest_id();
    let (plane, mut writer) = stand_in();
    // SAFETY: the old interface duplicates live descriptors for queued work.
    assert_eq!(
        unsafe { sessions.submit_dmabuf("alice", id, &frame(&plane, None, 1), 0) },
        STATUS_OK
    );
    drop(plane);
    entry.recv_timeout(Duration::from_secs(3)).unwrap();
    let shared = Arc::new(Mutex::new(sessions));
    let registration = Registration::new(shared.clone());
    let binding = bind_lease_encoder(&registration, "alice") as usize;
    let (done, completion) = mpsc::channel();
    let cancel = std::thread::spawn(move || {
        // SAFETY: this thread exclusively owns the binding through release;
        // the registration and shared sessions outlive the thread join.
        unsafe {
            let api = &*(binding as *const EncoderApiV1_2);
            (api.cancel_dmabuf_leases)(api.base.context, id);
            api.base.release.unwrap()(api.base.context);
        }
        done.send(()).unwrap();
    });
    assert_eq!(completion.try_recv(), Err(TryRecvError::Empty));
    assert!(!reader_closed(&mut writer));
    drop(release_gate);
    completion.recv_timeout(Duration::from_secs(3)).unwrap();
    cancel.join().unwrap();
    assert!(shared.lock().unwrap().status("alice", id).is_err());
    assert!(reader_closed(&mut writer));
}

struct ImmediateGpuPacket(Option<Packet>);
impl Backend for ImmediateGpuPacket {
    fn submit(&mut self, _: Vec<u8>, _: i64, _: bool) -> Result<SubmitResult, &'static str> {
        unreachable!()
    }
    fn receive(&mut self) -> Result<ReceiveResult, &'static str> {
        Ok(ReceiveResult::Ready(self.0.take()))
    }
    fn drain(&mut self) -> Result<bool, &'static str> {
        Ok(true)
    }
    fn input_support(&self) -> InputSupport {
        InputSupport {
            gpu_bgra_conversion: true,
            gpu_scaling: false,
            dmabuf: Some(import_support()),
            cuda_device: None,
        }
    }
    fn supports_gpu_completion(&self) -> bool {
        true
    }
    fn submit_gpu(
        &mut self,
        input: GpuInput,
        timestamp: i64,
        _: bool,
    ) -> Result<SubmitResult, &'static str> {
        drop(input);
        self.0 = Some(Packet {
            ready_at: None,
            bytes: vec![1, 2, 3],
            timestamp_100ns: timestamp,
            keyframe: false,
        });
        Ok(SubmitResult::Accepted)
    }
}
#[test]
fn encoded_output_retires_the_retained_reference_before_session_close() {
    use std::sync::atomic::Ordering;
    let mut sessions = Sessions::new(true);
    sessions
        .insert(
            "alice",
            configuration(),
            Box::new(ImmediateGpuPacket(None)),
            false,
        )
        .unwrap();
    let id = sessions.latest_id();
    let (plane, _writer) = stand_in();
    let callbacks = LeaseCallbacks::default();
    let lease = leased_frame(frame(&plane, None, 1), &callbacks);
    // SAFETY: descriptors and callbacks remain alive through close.
    assert_eq!(
        unsafe { sessions.submit_dmabuf_leased("alice", id, &lease, 0) },
        STATUS_OK
    );
    assert_eq!(callbacks.retained.load(Ordering::Relaxed), 1);
    assert_eq!(callbacks.released.load(Ordering::Relaxed), 0);
    unsafe extern "C" fn accept(_: *mut std::ffi::c_void, _: *const EncodedPacket) -> Status {
        STATUS_OK
    }
    // SAFETY: the sink does not dereference its unused context.
    assert_eq!(
        unsafe {
            sessions.receive(
                "alice",
                id,
                PacketSink {
                    context: std::ptr::null_mut(),
                    write: accept,
                },
            )
        },
        STATUS_OK
    );
    assert_eq!(callbacks.released.load(Ordering::Relaxed), 1);
    sessions.close("alice", id).unwrap();
    assert_eq!(callbacks.released.load(Ordering::Relaxed), 1);
}

#[test]
fn provider_backpressure_does_not_queue_or_release_an_unowned_reference() {
    unsafe extern "C" fn full(_: *mut std::ffi::c_void) -> Status {
        MEDIA_BUSY
    }
    let mut sessions = Sessions::new(true);
    sessions
        .insert(
            "alice",
            configuration(),
            Box::new(ImmediateGpuPacket(None)),
            false,
        )
        .unwrap();
    let id = sessions.latest_id();
    let (plane, _writer) = stand_in();
    let callbacks = LeaseCallbacks::default();
    let mut lease = leased_frame(frame(&plane, None, 1), &callbacks);
    lease.retain = Some(full);
    // SAFETY: callback state and descriptor remain live; full retains nothing.
    assert_eq!(
        unsafe { sessions.submit_dmabuf_leased("alice", id, &lease, 0) },
        MEDIA_BUSY
    );
    assert!(sessions.values[&id].metadata.is_empty());
    assert_no_retained_lease(&callbacks);
}

#[derive(Default)]
struct ResourceCallbacks {
    retained: std::sync::atomic::AtomicU32,
    released: std::sync::atomic::AtomicU32,
}

unsafe extern "C" fn resource_retain(context: *mut std::ffi::c_void) -> Status {
    let callbacks = unsafe { &*context.cast::<ResourceCallbacks>() };
    callbacks
        .retained
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    STATUS_OK
}

unsafe extern "C" fn resource_release(
    context: *mut std::ffi::c_void,
    completion: *const FrameDescriptor,
) {
    assert_eq!(unsafe { (*completion).kind }, FRAME_SYNC_NONE);
    let callbacks = unsafe { &*context.cast::<ResourceCallbacks>() };
    callbacks
        .released
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

const CUDA_DEVICE: [u8; 16] = [5; 16];

fn cuda_harness() -> Harness {
    let mut harness = harness(InputSupport {
        gpu_bgra_conversion: false,
        gpu_scaling: false,
        dmabuf: None,
        cuda_device: Some(CUDA_DEVICE),
    });
    // Resource input requires a backend that orders completion with output.
    let mut sessions = Sessions::new(true);
    let backend = GpuFake {
        support: InputSupport {
            gpu_bgra_conversion: false,
            gpu_scaling: false,
            dmabuf: None,
            cuda_device: Some(CUDA_DEVICE),
        },
        capacity: harness.capacity.clone(),
        observed: harness.observed.clone(),
        completion: true,
    };
    sessions
        .insert("alice", configuration(), Box::new(backend), false)
        .unwrap();
    harness.id = sessions.latest_id();
    harness.sessions = sessions;
    harness
}

unsafe extern "C" fn collect_offers(
    context: *mut std::ffi::c_void,
    offer: *const CaptureResourceOffer,
) -> Status {
    let offers = unsafe { &mut *context.cast::<Vec<(u64, CudaDeviceCapabilities)>>() };
    let offer = unsafe { &*offer };
    let resource = unsafe { cuda_device_capabilities(&offer.resource) }.copied();
    offers.push((offer.id, resource.expect("CUDA offer payload")));
    STATUS_OK
}

fn offers(harness: &mut Harness) -> (u64, Vec<(u64, CudaDeviceCapabilities)>) {
    let mut offers = Vec::new();
    let mut generation = 0;
    let sink = CaptureResourceOfferSink {
        context: (&mut offers as *mut Vec<(u64, CudaDeviceCapabilities)>).cast(),
        write: collect_offers,
    };
    let status = unsafe {
        harness
            .sessions
            .enumerate_resources("alice", harness.id, sink, &mut generation)
    };
    assert_eq!(status, STATUS_OK);
    (generation, offers)
}

struct Producer {
    resource: CudaDeviceCapabilities,
    event: CudaEventCapabilities,
}

impl Producer {
    fn new(device: [u8; 16]) -> Self {
        Self {
            resource: CudaDeviceCapabilities {
                device_uuid: device,
                format: CUDA_FORMAT_X2B10G10R10,
                min_width: 16,
                max_width: 8192,
                min_height: 16,
                max_height: 8192,
                reserved: 0,
            },
            event: CudaEventCapabilities {
                device_uuid: device,
            },
        }
    }
    fn offer(&self, id: u64) -> CaptureResourceOffer {
        CaptureResourceOffer {
            id,
            ownership: FRAME_OWNERSHIP_RETAINABLE,
            accessibility: FRAME_ACCESS_GPU,
            reserved: 0,
            resource: cuda_descriptor(FRAME_RESOURCE_CUDA_DEVICE, &self.resource),
            acquire_sync: cuda_descriptor(FRAME_SYNC_CUDA_EVENT, &self.event),
            completion_sync: FrameDescriptor {
                kind: FRAME_SYNC_NONE,
                version: 0,
                size: 0,
                reserved: 0,
                namespace: [0; 16],
                data: std::ptr::null(),
            },
        }
    }
}

struct ResourceFrame {
    pixels: CudaDeviceFrame,
    ready: CudaEventSync,
}

impl ResourceFrame {
    fn new() -> Self {
        Self {
            pixels: CudaDeviceFrame {
                device_uuid: CUDA_DEVICE,
                device_pointer: 0x1000,
                pitch: 64,
                format: CUDA_FORMAT_X2B10G10R10,
                width: 16,
                height: 16,
            },
            ready: CudaEventSync {
                event: std::ptr::dangling_mut::<u8>().cast(),
            },
        }
    }
    fn frame(
        &self,
        generation: u64,
        offer: u64,
        sequence: u64,
        callbacks: &ResourceCallbacks,
    ) -> CaptureResourceFrame {
        CaptureResourceFrame {
            offer_generation: generation,
            offer_id: offer,
            timestamp_us: sequence * 1000,
            capture_pacing_timestamp_us: 0,
            sequence,
            width: 16,
            height: 16,
            freshness: CAPTURE_FRAME_FRESH,
            reserved: 0,
            color: FrameColorMetadata::default(),
            clock: FrameClockMetadata::default(),
            pacing_clock: FrameClockMetadata::default(),
            resource: cuda_descriptor(FRAME_RESOURCE_CUDA_DEVICE, &self.pixels),
            acquire_sync: cuda_descriptor(FRAME_SYNC_CUDA_EVENT, &self.ready),
            context: std::ptr::from_ref(callbacks).cast_mut().cast(),
            retain: resource_retain,
            release: resource_release,
        }
    }
}

#[test]
fn sessions_without_cuda_input_offer_no_resources() {
    let mut harness = gpu_harness();
    let (generation, offers) = offers(&mut harness);
    assert_ne!(generation, 0);
    assert!(offers.is_empty(), "lease-only sessions keep the lease path");
}

#[test]
fn cuda_resource_input_is_negotiated_retained_and_released_at_close() {
    use std::sync::atomic::Ordering;
    let mut harness = cuda_harness();
    let (generation, offers) = offers(&mut harness);
    assert_eq!(offers.len(), 1);
    let (offer_id, offered) = offers[0];
    assert_eq!(offered.device_uuid, CUDA_DEVICE);
    assert_eq!((offered.min_width, offered.max_width), (16, 16));
    let id = harness.id;
    let configure = |harness: &mut Harness, producer: &Producer| unsafe {
        harness.sessions.configure_resource(
            "alice",
            id,
            generation,
            offer_id,
            7,
            &producer.offer(3),
        )
    };
    let callbacks = ResourceCallbacks::default();
    let source = ResourceFrame::new();
    let submit = |harness: &mut Harness, frame: &CaptureResourceFrame, flags| unsafe {
        harness.sessions.submit_resource("alice", id, frame, flags)
    };
    // Another device's memory is never accepted, and nothing is configured.
    assert_eq!(
        configure(&mut harness, &Producer::new([6; 16])),
        MEDIA_UNSUPPORTED
    );
    assert_eq!(
        submit(&mut harness, &source.frame(7, 3, 1, &callbacks), 0),
        MEDIA_UNSUPPORTED
    );
    assert_eq!(
        configure(&mut harness, &Producer::new(CUDA_DEVICE)),
        STATUS_OK
    );
    // Unknown flags and stale offers consume nothing.
    assert_eq!(
        submit(&mut harness, &source.frame(7, 3, 1, &callbacks), 2),
        MEDIA_UNSUPPORTED
    );
    assert_eq!(
        submit(&mut harness, &source.frame(8, 3, 1, &callbacks), 0),
        MEDIA_UNSUPPORTED
    );
    assert_eq!(callbacks.retained.load(Ordering::Relaxed), 0);
    // A busy encoder accepts nothing either.
    *harness.capacity.lock().unwrap() = false;
    assert_eq!(
        submit(&mut harness, &source.frame(7, 3, 1, &callbacks), 0),
        MEDIA_BUSY
    );
    *harness.capacity.lock().unwrap() = true;
    assert_eq!(callbacks.retained.load(Ordering::Relaxed), 0);
    assert_eq!(
        submit(
            &mut harness,
            &source.frame(7, 3, 1, &callbacks),
            FRAME_RESOURCE_SUBMIT_FORCE_IDR
        ),
        STATUS_OK
    );
    assert_eq!(callbacks.retained.load(Ordering::Relaxed), 1);
    assert_eq!(callbacks.released.load(Ordering::Relaxed), 0);
    let observed = harness.observed.lock().unwrap().cuda.clone();
    assert_eq!(
        observed,
        vec![crate::input::CudaInput {
            pointer: 0x1000,
            pitch: 64,
            width: 16,
            height: 16,
            ready: source.ready.event as usize,
        }]
    );
    harness.sessions.close("alice", id).unwrap();
    assert_eq!(callbacks.released.load(Ordering::Relaxed), 1);
}
