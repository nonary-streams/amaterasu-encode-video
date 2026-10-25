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
}

struct GpuFake {
    support: InputSupport,
    capacity: Arc<Mutex<bool>>,
    observed: Arc<Mutex<Observed>>,
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
        dmabuf: Some(import_support()),
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
    let mut wrong_size = valid;
    wrong_size.width = 32;
    let mut stray_plane = valid;
    stray_plane.planes[1].fd = plane.as_raw_fd();
    let mut force_two = valid;
    force_two.sequence = 6;
    for (label, candidate, force) in [
        ("no planes", no_planes, 0),
        ("negative fd", negative_fd, 0),
        ("reserved", reserved, 0),
        ("dimensions", wrong_size, 0),
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
fn the_native_table_offers_dmabuf_submission_at_minor_one() {
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
        assert_eq!(api.base.header.minor, ENCODER_DMABUF_INTERFACE_MINOR);
        assert_eq!(api.base.header.size, ENCODER_API_V1_1_SIZE);
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
    assert_eq!(
        bind(ENCODER_DMABUF_INTERFACE_MINOR + 1, ENCODER_API_V1_1_SIZE).0,
        MEDIA_UNSUPPORTED
    );
    assert_eq!(bind(0, ENCODER_API_V1_1_SIZE + 8).0, MEDIA_UNSUPPORTED);
}

/// Holds every GPU input it receives, blocking the first until released.
struct HoldingBackend {
    gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
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
            dmabuf: Some(import_support()),
        }
    }
    fn submit_gpu(&mut self, input: GpuInput, _: i64, _: bool) -> Result<(), &'static str> {
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
    let mut worker = Worker::with_backend(HoldingBackend {
        gate: gate.clone(),
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
        assert!(accepted <= 3, "capacity must stay bounded");
    }
    assert!(accepted >= 1);
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
