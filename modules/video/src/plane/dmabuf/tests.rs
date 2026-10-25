use super::*;
use crate::model::{CaptureProvider, Configuration};
use crate::plane::FramePath;
use std::cell::{Cell, RefCell};

const XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

/// Shared state for one fake capture provider and one fake encoder session.
/// Both native tables point at the same harness; every field uses interior
/// mutability because the callbacks only receive a shared context pointer.
struct Harness {
    deliver_freshness: Cell<Option<u32>>,
    capture_status: Cell<Status>,
    frame_width: Cell<u32>,
    plane_count: Cell<u32>,
    encoder_status: Cell<Status>,
    in_capture_callback: Cell<bool>,
    submitted: RefCell<Vec<SubmittedFrame>>,
    presentation: Cell<PresentationStatus>,
    presentation_status: Cell<Status>,
}

#[derive(Debug, PartialEq, Eq)]
struct SubmittedFrame {
    inside_capture_callback: bool,
    fd: i32,
    sequence: u64,
    timestamp_us: u64,
    force_idr: u32,
}

impl Default for Harness {
    fn default() -> Self {
        Self {
            deliver_freshness: Cell::new(Some(CAPTURE_FRAME_FRESH)),
            capture_status: Cell::new(STATUS_OK),
            frame_width: Cell::new(16),
            plane_count: Cell::new(1),
            encoder_status: Cell::new(STATUS_OK),
            in_capture_callback: Cell::new(false),
            submitted: RefCell::new(Vec::new()),
            presentation: Cell::new(PresentationStatus::default()),
            presentation_status: Cell::new(STATUS_OK),
        }
    }
}

fn frame(harness: &Harness) -> DmaBufFrame {
    let mut planes = [DmaBufPlane::UNUSED; DMABUF_MAX_PLANES];
    if harness.plane_count.get() > 0 {
        planes[0] = DmaBufPlane {
            fd: 7,
            acquire_fence_fd: -1,
            offset: 0,
            pitch: 64,
        };
    }
    DmaBufFrame {
        width: harness.frame_width.get(),
        height: 16,
        fourcc: XRGB8888,
        plane_count: harness.plane_count.get(),
        modifier: DRM_FORMAT_MOD_INVALID,
        planes,
        color: DMABUF_COLOR_SRGB,
        reserved: 0,
        timestamp_us: 900,
        sequence: 44,
    }
}

unsafe extern "C" fn read_dmabuf(context: *mut c_void, _: u64, sink: DmaBufFrameSink) -> Status {
    // SAFETY: the test passes a live harness as the capture table context.
    let harness = unsafe { &*context.cast::<Harness>() };
    if let Some(freshness) = harness.deliver_freshness.get() {
        let frame = frame(harness);
        harness.in_capture_callback.set(true);
        // SAFETY: the stack frame and its descriptor numbers outlive this call.
        // The sink is called directly so unvalidated metadata reaches video.
        let status = unsafe { (sink.write)(sink.context, &frame, freshness) };
        harness.in_capture_callback.set(false);
        if status != STATUS_OK {
            return status;
        }
    }
    harness.capture_status.get()
}

unsafe extern "C" fn submit_dmabuf(
    context: *mut c_void,
    _: u64,
    frame: *const DmaBufFrame,
    force_idr: u32,
) -> Status {
    // SAFETY: the encoder context is the live harness and `frame` is borrowed
    // for this synchronous call.
    let harness = unsafe { &*context.cast::<Harness>() };
    let frame = unsafe { &*frame };
    harness.submitted.borrow_mut().push(SubmittedFrame {
        inside_capture_callback: harness.in_capture_callback.get(),
        fd: frame.planes[0].fd,
        sequence: frame.sequence,
        timestamp_us: frame.timestamp_us,
        force_idr,
    });
    harness.encoder_status.get()
}

unsafe extern "C" fn no_cpu_frame(_: *mut c_void, _: u64, _: FrameSink) -> Status {
    MEDIA_UNSUPPORTED
}
unsafe extern "C" fn no_geometry(_: *mut c_void, _: u64, _: *mut CaptureGeometry) -> Status {
    MEDIA_UNSUPPORTED
}
unsafe extern "C" fn no_fresh_frame(_: *mut c_void, _: u64, _: FreshFrameSinkV1_2) -> Status {
    MEDIA_UNSUPPORTED
}
unsafe extern "C" fn no_timed_frame(_: *mut c_void, _: u64, _: FreshFrameSinkV1_3) -> Status {
    MEDIA_UNSUPPORTED
}
unsafe extern "C" fn reject_cpu(_: *mut c_void, _: u64, _: *const CpuFrame, _: u32) -> Status {
    MEDIA_UNSUPPORTED
}
unsafe extern "C" fn no_packet(_: *mut c_void, _: u64, _: PacketSink) -> Status {
    MEDIA_PENDING
}
unsafe extern "C" fn drained(_: *mut c_void, _: u64) -> Status {
    STATUS_OK
}

fn interface(size: u32, minor: u32, context: &Harness) -> NativeInterface {
    NativeInterface {
        header: AbiHeader {
            size,
            major: 1,
            minor,
        },
        context: (context as *const Harness).cast_mut().cast(),
        release: None,
    }
}

fn capture_api(harness: &Harness) -> CaptureApiV1_4 {
    CaptureApiV1_4 {
        base: interface(
            CAPTURE_API_V1_4_SIZE,
            CAPTURE_DMABUF_INTERFACE_MINOR,
            harness,
        ),
        read_frame: no_cpu_frame,
        geometry: no_geometry,
        read_frame_with_freshness: no_fresh_frame,
        read_frame_with_capture_timestamp: no_timed_frame,
        read_dmabuf_frame: read_dmabuf,
    }
}

unsafe extern "C" fn latest(
    context: *mut c_void,
    _: u64,
    output: *mut PresentationStatus,
) -> Status {
    // SAFETY: the capture context is the live harness and `output` is the
    // caller's writable stack value for this synchronous call.
    let harness = unsafe { &*context.cast::<Harness>() };
    unsafe { output.write(harness.presentation.get()) };
    harness.presentation_status.get()
}

fn presentation_capture_api(harness: &Harness) -> CaptureApiV1_5 {
    CaptureApiV1_5 {
        base: interface(
            CAPTURE_API_V1_5_SIZE,
            CAPTURE_PRESENTATION_INTERFACE_MINOR,
            harness,
        ),
        read_frame: no_cpu_frame,
        geometry: no_geometry,
        read_frame_with_freshness: no_fresh_frame,
        read_frame_with_capture_timestamp: no_timed_frame,
        read_dmabuf_frame: read_dmabuf,
        latest_presentation: latest,
    }
}

fn encoder_api(harness: &Harness) -> EncoderApiV1_1 {
    EncoderApiV1_1 {
        base: interface(
            ENCODER_API_V1_1_SIZE,
            ENCODER_DMABUF_INTERFACE_MINOR,
            harness,
        ),
        submit: reject_cpu,
        receive: no_packet,
        drain: drained,
        submit_dmabuf,
    }
}

fn configuration() -> Configuration {
    serde_json::from_value(serde_json::json!({
        "width": 16, "height": 16, "fps": 60, "bitrate": 100_000
    }))
    .unwrap()
}

fn pump(configuration: &Configuration, capture_due: bool, force_idr: bool) -> Pump<'_> {
    Pump {
        video: 1,
        provider: CaptureProvider::Vkms,
        frame_path: FramePath::DmaBuf,
        capture: 3,
        encoder: 5,
        configuration,
        capture_due,
        constant_rate: false,
        repeat_due: !capture_due,
        repeat_frame: None,
        timestamp_us: 1_234,
        sequence: 9,
        force_idr,
    }
}

fn deliver_once(harness: &Harness, capture_due: bool) -> Result<Progress, &'static str> {
    let configuration = configuration();
    deliver(
        &capture_api(harness),
        &encoder_api(harness),
        &pump(&configuration, capture_due, false),
        !capture_due,
    )
}

#[test]
fn tails_require_both_advertised_minor_and_complete_table_size() {
    let header = |size, minor| AbiHeader {
        size,
        major: 1,
        minor,
    };
    assert!(capture_tail_available(header(
        CAPTURE_API_V1_4_SIZE,
        CAPTURE_DMABUF_INTERFACE_MINOR
    )));
    assert!(!capture_tail_available(header(
        CAPTURE_API_V1_3_SIZE,
        CAPTURE_DMABUF_INTERFACE_MINOR
    )));
    assert!(!capture_tail_available(header(
        CAPTURE_API_V1_4_SIZE,
        CAPTURE_PACING_TIMESTAMP_INTERFACE_MINOR
    )));
    assert!(encoder_tail_available(header(
        ENCODER_API_V1_1_SIZE,
        ENCODER_DMABUF_INTERFACE_MINOR
    )));
    assert!(!encoder_tail_available(header(
        ENCODER_API_V1_0_SIZE,
        ENCODER_DMABUF_INTERFACE_MINOR
    )));
    assert!(!encoder_tail_available(header(ENCODER_API_V1_1_SIZE, 0)));

    let capture = header(CAPTURE_API_V1_4_SIZE, CAPTURE_DMABUF_INTERFACE_MINOR);
    let encoder = header(ENCODER_API_V1_1_SIZE, ENCODER_DMABUF_INTERFACE_MINOR);
    assert_eq!(path_unavailable(capture, encoder), None);
    assert_eq!(
        path_unavailable(header(CAPTURE_API_V1_3_SIZE, 3), encoder),
        Some("capture_dmabuf_interface_unavailable")
    );
    assert_eq!(
        path_unavailable(capture, header(ENCODER_API_V1_0_SIZE, 0)),
        Some("encoder_dmabuf_interface_unavailable")
    );
}

#[test]
fn borrowed_frame_is_submitted_inside_the_capture_callback_with_video_clock() {
    let harness = Harness::default();
    let configuration = configuration();
    let progress = deliver(
        &capture_api(&harness),
        &encoder_api(&harness),
        &pump(&configuration, true, true),
        false,
    )
    .unwrap();
    assert!(progress.submitted);
    assert!(!progress.dropped);
    assert!(!progress.repeat_attempted);
    assert_eq!(progress.dmabuf_fresh, Some(true));
    assert!(!progress.capture_reused);
    assert_eq!(progress.dmabuf_fallback, None);
    assert!(progress.owned_frame.is_none());
    assert_eq!(progress.capture_frame_count, 1);
    let metadata = progress.frame.unwrap();
    assert_eq!(
        (metadata.width, metadata.stride, metadata.sequence),
        (16, 64, 9)
    );
    assert_eq!(
        *harness.submitted.borrow(),
        [SubmittedFrame {
            inside_capture_callback: true,
            fd: 7,
            sequence: 9,
            timestamp_us: 1_234,
            force_idr: 1,
        }]
    );
}

#[test]
fn encoder_backpressure_drops_the_presentation_without_fallback() {
    let harness = Harness::default();
    harness.encoder_status.set(MEDIA_BUSY);
    let progress = deliver_once(&harness, true).unwrap();
    assert!(progress.dropped);
    assert!(!progress.submitted);
    assert!(progress.frame.is_none());
    assert_eq!(progress.dmabuf_fallback, None);
}

#[test]
fn unsupported_capture_or_encoder_requests_cpu_fallback() {
    let harness = Harness::default();
    harness.encoder_status.set(MEDIA_UNSUPPORTED);
    let progress = deliver_once(&harness, true).unwrap();
    assert!(!progress.submitted && !progress.dropped);
    assert_eq!(progress.dmabuf_fallback, Some("encoder_dmabuf_unsupported"));

    let harness = Harness::default();
    harness.deliver_freshness.set(None);
    harness.capture_status.set(MEDIA_UNSUPPORTED);
    let progress = deliver_once(&harness, true).unwrap();
    assert_eq!(progress.dmabuf_fallback, Some("capture_dmabuf_unsupported"));
    assert!(harness.submitted.borrow().is_empty());

    let harness = Harness::default();
    harness.frame_width.set(32);
    let progress = deliver_once(&harness, true).unwrap();
    assert_eq!(progress.dmabuf_fallback, Some("dmabuf_size_mismatch"));
    assert!(harness.submitted.borrow().is_empty());
}

#[test]
fn cached_reuse_repeat_is_counted_as_reuse_and_pending_reads_submit_nothing() {
    let harness = Harness::default();
    harness
        .deliver_freshness
        .set(Some(CAPTURE_FRAME_CACHED_REUSE));
    let progress = deliver_once(&harness, false).unwrap();
    assert!(progress.submitted);
    assert!(progress.repeat_attempted);
    assert!(progress.capture_reused);
    assert_eq!(progress.dmabuf_fresh, Some(false));

    let harness = Harness::default();
    harness.deliver_freshness.set(None);
    harness.capture_status.set(MEDIA_PENDING);
    let progress = deliver_once(&harness, true).unwrap();
    assert!(!progress.submitted && !progress.dropped);
    assert_eq!(progress.capture_frame_count, 0);
    assert_eq!(progress.dmabuf_fallback, None);
}

#[test]
fn invalid_frames_and_capture_errors_are_terminal() {
    let harness = Harness::default();
    harness.plane_count.set(0);
    assert_eq!(deliver_once(&harness, true).err(), Some("frame_rejected"));
    assert!(harness.submitted.borrow().is_empty());

    let harness = Harness::default();
    harness.encoder_status.set(MEDIA_BACKEND_ERROR);
    assert_eq!(deliver_once(&harness, true).err(), Some("frame_rejected"));

    let harness = Harness::default();
    harness.deliver_freshness.set(None);
    harness.capture_status.set(MEDIA_BACKEND_ERROR);
    assert_eq!(deliver_once(&harness, true).err(), Some("capture_failed"));
}

#[test]
fn presentation_tail_requires_v1_5_minor_and_complete_table_size() {
    let header = |size, minor| AbiHeader {
        size,
        major: 1,
        minor,
    };
    assert!(presentation_tail_available(header(
        CAPTURE_API_V1_5_SIZE,
        CAPTURE_PRESENTATION_INTERFACE_MINOR
    )));
    assert!(presentation_tail_available(header(
        CAPTURE_API_V1_5_SIZE + 8,
        CAPTURE_PRESENTATION_INTERFACE_MINOR + 1
    )));
    // A v1.5 minor on a v1.4-sized table must not expose the query.
    assert!(!presentation_tail_available(header(
        CAPTURE_API_V1_4_SIZE,
        CAPTURE_PRESENTATION_INTERFACE_MINOR
    )));
    assert!(!presentation_tail_available(header(
        CAPTURE_API_V1_5_SIZE,
        CAPTURE_DMABUF_INTERFACE_MINOR
    )));
    // A v1.4 provider still qualifies for the DMA-BUF path itself.
    assert!(capture_tail_available(header(
        CAPTURE_API_V1_4_SIZE,
        CAPTURE_DMABUF_INTERFACE_MINOR
    )));
}

#[test]
fn presentation_query_maps_provider_status() {
    let harness = Harness::default();
    let api = presentation_capture_api(&harness);
    assert_eq!(latest_presentation(&api, 3), Ok(PresentationQuery::Pending));

    let presented = PresentationStatus {
        sequence: 12,
        timestamp_us: 4_500,
    };
    harness.presentation.set(presented);
    assert_eq!(
        latest_presentation(&api, 3),
        Ok(PresentationQuery::Latest(presented))
    );
    for (status, expected) in [
        (MEDIA_PENDING, Ok(PresentationQuery::Pending)),
        (MEDIA_BUSY, Ok(PresentationQuery::Pending)),
        (MEDIA_UNSUPPORTED, Ok(PresentationQuery::Unavailable)),
        (MEDIA_CLOSED, Err("capture_failed")),
        (MEDIA_BACKEND_ERROR, Err("capture_failed")),
    ] {
        harness.presentation_status.set(status);
        assert_eq!(latest_presentation(&api, 3), expected, "{status}");
    }
    // Querying never delivers a frame.
    assert!(harness.submitted.borrow().is_empty());
}

#[test]
fn delivery_reports_the_provider_sequence_it_delivered() {
    let harness = Harness::default();
    let progress = deliver_once(&harness, true).unwrap();
    assert_eq!(progress.dmabuf_presentation, Some(44));
    // The encoder still receives video's own sequence.
    assert_eq!(harness.submitted.borrow()[0].sequence, 9);

    harness.encoder_status.set(MEDIA_BUSY);
    let dropped = deliver_once(&harness, true).unwrap();
    assert!(dropped.dropped);
    assert_eq!(dropped.dmabuf_presentation, Some(44));

    let harness = Harness::default();
    harness.deliver_freshness.set(None);
    harness.capture_status.set(MEDIA_PENDING);
    assert_eq!(
        deliver_once(&harness, true).unwrap().dmabuf_presentation,
        None
    );
}
