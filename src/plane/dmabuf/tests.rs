use super::*;

#[test]
fn lease_requirement_rejects_each_borrowed_native_table() {
    let capture = AbiHeader {
        size: CAPTURE_API_V1_6_SIZE,
        major: 1,
        minor: CAPTURE_LEASE_INTERFACE_MINOR,
    };
    let encoder = AbiHeader {
        size: ENCODER_API_V1_2_SIZE,
        major: 1,
        minor: ENCODER_LEASE_INTERFACE_MINOR,
    };
    assert_eq!(require_lease_path(capture, encoder), Ok(()));
    assert_eq!(
        require_lease_path(
            AbiHeader {
                minor: 5,
                ..capture
            },
            encoder
        ),
        Err("capture_frame_lease_required")
    );
    assert_eq!(
        require_lease_path(
            capture,
            AbiHeader {
                minor: 1,
                ..encoder
            }
        ),
        Err("encoder_frame_lease_required")
    );
}
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
    frame_color: Cell<u32>,
    resource_color: Cell<FrameColorMetadata>,
    plane_count: Cell<u32>,
    encoder_status: Cell<Status>,
    in_capture_callback: Cell<bool>,
    submitted: RefCell<Vec<SubmittedFrame>>,
    presentation: Cell<PresentationStatus>,
    presentation_status: Cell<Status>,
    retained: Cell<usize>,
    released: Cell<usize>,
    retain_status: Cell<Status>,
    pending_lease: RefCell<Option<DmaBufFrameLease>>,
    processor_mode: Cell<u8>,
    lifecycle_events: RefCell<Vec<&'static str>>,
    source_binding_alive: Cell<bool>,
    capture_reads: Cell<usize>,
    encoder_polls: Cell<usize>,
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
            frame_color: Cell::new(DMABUF_COLOR_SRGB),
            resource_color: Cell::new(FrameColorMetadata::default()),
            plane_count: Cell::new(1),
            encoder_status: Cell::new(STATUS_OK),
            in_capture_callback: Cell::new(false),
            submitted: RefCell::new(Vec::new()),
            presentation: Cell::new(PresentationStatus::default()),
            presentation_status: Cell::new(STATUS_OK),
            retained: Cell::new(0),
            released: Cell::new(0),
            retain_status: Cell::new(STATUS_OK),
            pending_lease: RefCell::new(None),
            processor_mode: Cell::new(0),
            lifecycle_events: RefCell::new(Vec::new()),
            source_binding_alive: Cell::new(true),
            capture_reads: Cell::new(0),
            encoder_polls: Cell::new(0),
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
        color: harness.frame_color.get(),
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
unsafe extern "C" fn no_packet(context: *mut c_void, _: u64, _: PacketSink) -> Status {
    let harness = unsafe { &*context.cast::<Harness>() };
    harness.encoder_polls.set(harness.encoder_polls.get() + 1);
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

unsafe extern "C" fn retain_lease(context: *mut c_void) -> Status {
    // SAFETY: test leases reference the live harness through every release.
    let harness = unsafe { &*context.cast::<Harness>() };
    let status = harness.retain_status.get();
    if status == STATUS_OK {
        harness.retained.set(harness.retained.get() + 1);
    }
    status
}

unsafe extern "C" fn release_lease(context: *mut c_void, fence: i32) {
    // SAFETY: test leases reference the live harness through every release.
    let harness = unsafe { &*context.cast::<Harness>() };
    assert_eq!(fence, -1);
    assert!(harness.retained.get() > 0);
    harness.retained.set(harness.retained.get() - 1);
    harness.released.set(harness.released.get() + 1);
}

fn lease(harness: &Harness) -> DmaBufFrameLease {
    DmaBufFrameLease {
        frame: frame(harness),
        context: (harness as *const Harness).cast_mut().cast(),
        retain: Some(retain_lease),
        release: Some(release_lease),
    }
}

unsafe extern "C" fn read_lease(context: *mut c_void, _: u64, sink: DmaBufLeaseSink) -> Status {
    // SAFETY: table context is the live harness and callback is synchronous.
    let harness = unsafe { &*context.cast::<Harness>() };
    harness.capture_reads.set(harness.capture_reads.get() + 1);
    if let Some(freshness) = harness.deliver_freshness.get() {
        harness.in_capture_callback.set(true);
        let status = unsafe { (sink.write)(sink.context, &lease(harness), freshness) };
        harness.in_capture_callback.set(false);
        if status != STATUS_OK {
            return status;
        }
    }
    harness.capture_status.get()
}

fn leased_capture_api(harness: &Harness) -> CaptureApiV1_6 {
    CaptureApiV1_6 {
        base: interface(
            CAPTURE_API_V1_6_SIZE,
            CAPTURE_LEASE_INTERFACE_MINOR,
            harness,
        ),
        read_frame: no_cpu_frame,
        geometry: no_geometry,
        read_frame_with_freshness: no_fresh_frame,
        read_frame_with_capture_timestamp: no_timed_frame,
        read_dmabuf_frame: read_dmabuf,
        latest_presentation: latest,
        read_dmabuf_frame_leased: read_lease,
    }
}

unsafe extern "C" fn submit_lease(
    context: *mut c_void,
    stream: u64,
    input: *const DmaBufFrameLease,
    force_idr: u32,
) -> Status {
    // SAFETY: input and context are live for this synchronous test callback.
    let harness = unsafe { &*context.cast::<Harness>() };
    let input = unsafe { &*input };
    let status = unsafe { submit_dmabuf(context, stream, &input.frame, force_idr) };
    if status == STATUS_OK {
        let retained = unsafe { input.retain.unwrap()(input.context) };
        if retained != STATUS_OK {
            return retained;
        }
        assert!(harness.pending_lease.borrow().is_none());
        *harness.pending_lease.borrow_mut() = Some(*input);
    }
    status
}

fn leased_encoder_api(harness: &Harness) -> EncoderApiV1_2 {
    EncoderApiV1_2 {
        base: interface(
            ENCODER_API_V1_2_SIZE,
            ENCODER_LEASE_INTERFACE_MINOR,
            harness,
        ),
        submit: reject_cpu,
        receive: no_packet,
        drain: drained,
        submit_dmabuf,
        submit_dmabuf_leased: submit_lease,
        cancel_dmabuf_leases: cancel_encoder_leases,
    }
}

unsafe extern "C" fn cancel_encoder_leases(context: *mut c_void, _: u64) {
    // SAFETY: the fixture and outstanding lease remain live through retirement.
    let harness = unsafe { &*context.cast::<Harness>() };
    assert!(harness.source_binding_alive.get());
    harness.lifecycle_events.borrow_mut().push("encoder_cancel");
    if harness.pending_lease.borrow().is_some() {
        finish_encoder(harness);
    }
}

fn finish_encoder(harness: &Harness) {
    let held = harness.pending_lease.borrow_mut().take().unwrap();
    // SAFETY: the encoder owns one retained reference and completes its reads.
    unsafe { held.release.unwrap()(held.context, -1) };
}

#[test]
fn lease_and_encoder_tails_require_complete_advertised_prefixes() {
    let header = |size, minor| AbiHeader {
        size,
        major: 1,
        minor,
    };
    assert!(lease_tail_available(header(
        CAPTURE_API_V1_6_SIZE,
        CAPTURE_LEASE_INTERFACE_MINOR
    )));
    assert!(!lease_tail_available(header(
        CAPTURE_API_V1_5_SIZE,
        CAPTURE_LEASE_INTERFACE_MINOR
    )));
    assert!(!lease_tail_available(header(
        CAPTURE_API_V1_6_SIZE,
        CAPTURE_PRESENTATION_INTERFACE_MINOR
    )));
    assert!(encoder_lease_tail_available(header(
        ENCODER_API_V1_2_SIZE,
        ENCODER_LEASE_INTERFACE_MINOR
    )));
    assert!(!encoder_lease_tail_available(header(
        ENCODER_API_V1_1_SIZE,
        ENCODER_LEASE_INTERFACE_MINOR
    )));
    assert_eq!(
        path_unavailable(
            header(CAPTURE_API_V1_6_SIZE, CAPTURE_LEASE_INTERFACE_MINOR),
            header(ENCODER_API_V1_1_SIZE, ENCODER_DMABUF_INTERFACE_MINOR)
        ),
        None
    );
}

#[test]
fn encoder_keeps_immutable_lease_after_capture_callback_and_releases_on_completion() {
    let harness = Harness::default();
    let config = configuration();
    let progress = deliver_leased(
        &leased_capture_api(&harness),
        &leased_encoder_api(&harness),
        &pump(&config, true, true),
        false,
    )
    .unwrap();
    assert!(progress.submitted);
    assert_eq!(
        harness.retained.get(),
        1,
        "only encoder reference remains after callback"
    );
    assert_eq!(
        harness.released.get(),
        0,
        "video lends the existing capture reference without an extra retain"
    );
    let held = *harness.pending_lease.borrow();
    assert_eq!(held.unwrap().frame.sequence, 9);
    assert_eq!(held.unwrap().frame.timestamp_us, 1234);
    assert!(!harness.in_capture_callback.get());
    finish_encoder(&harness);
    assert_eq!(harness.retained.get(), 0);
    assert_eq!(harness.released.get(), 1);
}

#[test]
fn leased_delivery_releases_on_encoder_busy_failure_and_retain_refusal() {
    for status in [MEDIA_BUSY, MEDIA_BACKEND_ERROR, MEDIA_UNSUPPORTED] {
        let harness = Harness::default();
        harness.encoder_status.set(status);
        let config = configuration();
        let result = deliver_leased(
            &leased_capture_api(&harness),
            &leased_encoder_api(&harness),
            &pump(&config, true, false),
            false,
        );
        assert_eq!(harness.retained.get(), 0);
        assert_eq!(harness.released.get(), 0);
        assert!(harness.pending_lease.borrow().is_none());
        if status == MEDIA_BUSY {
            assert!(result.unwrap().dropped);
        } else if status == MEDIA_BACKEND_ERROR {
            assert_eq!(result.err(), Some("frame_rejected"));
        } else {
            assert_eq!(
                result.unwrap().dmabuf_fallback,
                Some("encoder_dmabuf_lease_unsupported")
            );
        }
    }
    let harness = Harness::default();
    harness.retain_status.set(MEDIA_BUSY);
    let config = configuration();
    let result = deliver_leased(
        &leased_capture_api(&harness),
        &leased_encoder_api(&harness),
        &pump(&config, true, false),
        false,
    )
    .unwrap();
    assert!(result.dropped);
    assert_eq!(
        harness.submitted.borrow().len(),
        1,
        "the encoder attempts retention directly; video adds no intermediate reference"
    );
    assert_eq!(harness.retained.get(), 0);
    assert_eq!(harness.released.get(), 0);
}

#[test]
fn disabled_processing_cannot_claim_a_different_output_range() {
    let mut config = configuration();
    config.video_stream_format = serde_json::from_value(serde_json::json!({
        "codec":"hevc", "bit_depth":10, "dynamic_range":"hdr"
    }))
    .unwrap();
    config.gpu_processing = Some(
        serde_json::from_value(serde_json::json!({
            "service":"fixture.processor/1", "enabled":false,
            "source_hdr_required":false, "settings":{}
        }))
        .unwrap(),
    );
    assert!(!config.validate());
}

#[test]
fn borrowed_capture_also_checks_selected_source_color() {
    let harness = Harness::default();
    let mut config = configuration();
    config.gpu_processing = Some(
        serde_json::from_value(serde_json::json!({
            "service":"fixture.processor/1", "enabled":false,
            "source_hdr_required":true, "settings":{}
        }))
        .unwrap(),
    );
    let result = deliver(
        &capture_api(&harness),
        &encoder_api(&harness),
        &pump(&config, true, false),
        false,
    );
    assert_eq!(result.err(), Some("frame_rejected"));
    assert!(harness.submitted.borrow().is_empty());
}

struct RuntimeFixture<'a> {
    harness: &'a Harness,
    capture: usize,
    encoder: usize,
    external_encoder: usize,
    processor: usize,
}

unsafe extern "C" fn invoke_lifecycle_runtime(
    context: *mut c_void,
    input: BorrowedBytes,
    sink: ReplySink,
) -> Status {
    use amaterasu_module_sdk::{Command, Response};
    // SAFETY: each test retains fixture and native tables through plane Drop.
    let fixture = unsafe { &*context.cast::<RuntimeFixture<'_>>() };
    let command: Command = serde_json::from_slice(unsafe { input.read() }.unwrap()).unwrap();
    let response = match command {
        Command::NativeBindTyped {
            service, contract, ..
        } => {
            let expected_contract = match service.as_str() {
                super::super::ENCODE | "example.encode/1" => {
                    amaterasu_module_sdk::media::native_contracts::ENCODER
                }
                "fixture.gpu-processor/1" => {
                    amaterasu_module_sdk::media::native_contracts::FRAME_PROCESSOR
                }
                "amaterasu.capture-vkms/1" => {
                    amaterasu_module_sdk::media::native_contracts::CAPTURE
                }
                _ => panic!("unexpected fixture service {service}"),
            };
            assert_eq!(contract, expected_contract);
            let (binding, address) = match service.as_str() {
                super::super::ENCODE => (1, fixture.encoder),
                "example.encode/1" => (4, fixture.external_encoder),
                "fixture.gpu-processor/1" => (2, fixture.processor),
                "amaterasu.capture-vkms/1" => (3, fixture.capture),
                _ => panic!("unexpected fixture service {service}"),
            };
            Response::NativeBound { binding, address }
        }
        Command::NativeRelease { binding } => {
            let event = match binding {
                1 => "encoder_release",
                2 => "processor_release",
                4 => "external_encoder_release",
                3 => {
                    assert_eq!(
                        fixture.harness.retained.get(),
                        0,
                        "capture binding released while downstream leases remain"
                    );
                    fixture.harness.source_binding_alive.set(false);
                    "capture_release"
                }
                _ => panic!("unexpected fixture binding"),
            };
            fixture.harness.lifecycle_events.borrow_mut().push(event);
            Response::Ok
        }
        _ => panic!("unexpected fixture command"),
    };
    // SAFETY: the synchronous runtime call keeps reply sink live through send.
    unsafe { sink.send(&serde_json::to_vec(&response).unwrap()) };
    STATUS_OK
}

unsafe extern "C" fn fixture_hdr_metadata(_: *mut c_void, _: u64, _: *mut HdrMetadata) -> Status {
    MEDIA_PENDING
}

unsafe extern "C" fn fixture_color_provenance(
    context: *mut c_void,
    stream: u64,
    out: *mut u32,
) -> Status {
    let harness = unsafe { &*context.cast::<Harness>() };
    assert_eq!(stream, 3);
    match harness.processor_mode.get() {
        10 => MEDIA_PENDING,
        16 => MEDIA_BUSY,
        17 => {
            unsafe { *out = CAPTURE_COLOR_PER_PRESENTATION_AUTHORITATIVE | 2 };
            STATUS_OK
        }
        11 => {
            unsafe { *out = 0 };
            STATUS_OK
        }
        12 => {
            unsafe { *out = CAPTURE_COLOR_PER_PRESENTATION_AUTHORITATIVE };
            STATUS_OK
        }
        _ => MEDIA_UNSUPPORTED,
    }
}

#[test]
fn selected_processing_requires_capture_color_provenance_before_delivery() {
    use crate::plane::{NativePlane, Plane};
    use amaterasu_module_sdk::RuntimeClient;
    use std::sync::{Arc, Mutex};
    for mode in [10, 11, 12, 13, 14, 15, 16, 17] {
        let harness = Harness::default();
        harness.processor_mode.set(mode);
        let mut base = leased_capture_api(&harness);
        base.base.header = AbiHeader {
            size: std::mem::size_of::<CaptureApiV1_10>() as u32,
            major: INTERFACE_MAJOR,
            minor: CAPTURE_COLOR_PROVENANCE_INTERFACE_MINOR,
        };
        if mode == 14 {
            base.base.header.minor = 9;
        } else if mode == 15 {
            base.base.header.size = std::mem::size_of::<CaptureApiV1_9>() as u32;
        }
        let capture = CaptureApiV1_10 {
            base: CaptureApiV1_9 {
                base: CaptureApiV1_7 {
                    base,
                    subscribe_events: no_subscribe,
                    unsubscribe_events: no_unsubscribe,
                    resolve_swap: no_swap,
                },
                enumerate_resources: resource_enumerate,
                select_resource: resource_select,
                read_resource_frame: resource_read,
                hdr_metadata: fixture_hdr_metadata,
            },
            color_provenance: fixture_color_provenance,
        };
        let encoder = leased_encoder_api(&harness);
        let fixture = RuntimeFixture {
            harness: &harness,
            capture: std::ptr::from_ref(&capture) as usize,
            encoder: std::ptr::from_ref(&encoder) as usize,
            external_encoder: 0,
            processor: 0,
        };
        let runtime = unsafe {
            RuntimeClient::from_endpoint(HostApi {
                header: AbiHeader::of::<HostApi>(),
                context: std::ptr::from_ref(&fixture).cast_mut().cast(),
                invoke: invoke_lifecycle_runtime,
            })
        };
        let mut plane = NativePlane::bind(
            &runtime,
            Arc::new(Mutex::new(crate::queue::Queues::default())),
            &[CaptureProvider::Vkms].into(),
            &Default::default(),
            &Default::default(),
            &Default::default(),
            None,
            None,
        )
        .unwrap();
        let mut config = configuration();
        config.gpu_processing = Some(
            serde_json::from_value(serde_json::json!({
                "service":"fixture.processor/1", "enabled":false,
                "source_hdr_required":false, "settings":{}
            }))
            .unwrap(),
        );
        let result = plane.pump(pump(&config, true, false));
        match mode {
            10 | 16 => assert!(!result.unwrap().submitted),
            12 => {
                assert!(result.unwrap().submitted);
                finish_encoder(&harness);
            }
            _ => assert_eq!(result.err(), Some("capture_color_provenance_unavailable")),
        }
        if mode != 12 {
            assert!(harness.submitted.borrow().is_empty());
        }
    }
}

unsafe extern "C" fn resource_enumerate(
    context: *mut c_void,
    _: u64,
    sink: CaptureResourceOfferSink,
    generation: *mut u64,
) -> Status {
    let harness = unsafe { &*context.cast::<Harness>() };
    let source = frame(harness);
    let caps = DmaBufResourceCapabilities {
        fourcc: source.fourcc,
        color: source.color,
        modifier: source.modifier,
        min_width: source.width,
        max_width: source.width,
        min_height: source.height,
        max_height: source.height,
        device_id: [0; 16],
    };
    let none = FrameDescriptor {
        kind: FRAME_SYNC_NONE,
        version: 0,
        size: 0,
        reserved: 0,
        namespace: [0; 16],
        data: std::ptr::null(),
    };
    let offer = CaptureResourceOffer {
        id: 7,
        ownership: FRAME_OWNERSHIP_RETAINABLE,
        accessibility: FRAME_ACCESS_GPU,
        reserved: 0,
        resource: FrameDescriptor {
            kind: FRAME_RESOURCE_DMABUF,
            version: 1,
            size: std::mem::size_of::<DmaBufResourceCapabilities>() as u32,
            data: (&caps as *const DmaBufResourceCapabilities).cast(),
            ..none
        },
        acquire_sync: none,
        completion_sync: FrameDescriptor {
            kind: FRAME_SYNC_SYNC_FILE,
            version: 1,
            ..none
        },
    };
    unsafe {
        *generation = 2;
        (sink.write)(sink.context, &offer)
    }
}
unsafe extern "C" fn resource_select(_: *mut c_void, _: u64, generation: u64, id: u64) -> Status {
    if generation == 2 && id == 7 {
        STATUS_OK
    } else {
        MEDIA_UNSUPPORTED
    }
}
unsafe extern "C" fn resource_retain(context: *mut c_void) -> Status {
    unsafe { retain_lease(context) }
}
unsafe extern "C" fn resource_release(context: *mut c_void, completion: *const FrameDescriptor) {
    let descriptor = unsafe { &*completion };
    assert_eq!(descriptor.kind, FRAME_SYNC_NONE);
    unsafe { release_lease(context, -1) };
}
unsafe extern "C" fn resource_read(
    context: *mut c_void,
    _: u64,
    sink: CaptureResourceFrameSink,
) -> Status {
    let harness = unsafe { &*context.cast::<Harness>() };
    let gpu = frame(harness);
    let none = FrameDescriptor {
        kind: FRAME_SYNC_NONE,
        version: 0,
        size: 0,
        reserved: 0,
        namespace: [0; 16],
        data: std::ptr::null(),
    };
    let wrapped = CaptureResourceFrame {
        offer_generation: 2,
        offer_id: 7,
        timestamp_us: gpu.timestamp_us,
        capture_pacing_timestamp_us: gpu.timestamp_us,
        sequence: gpu.sequence,
        width: gpu.width,
        height: gpu.height,
        freshness: CAPTURE_FRAME_FRESH,
        reserved: 0,
        color: harness.resource_color.get(),
        clock: Default::default(),
        pacing_clock: Default::default(),
        resource: FrameDescriptor {
            kind: FRAME_RESOURCE_DMABUF,
            version: 1,
            size: std::mem::size_of::<DmaBufFrame>() as u32,
            data: (&gpu as *const DmaBufFrame).cast(),
            ..none
        },
        acquire_sync: none,
        context,
        retain: resource_retain,
        release: resource_release,
    };
    harness.in_capture_callback.set(true);
    let status = unsafe { (sink.write)(sink.context, &wrapped) };
    harness.in_capture_callback.set(false);
    status
}
unsafe extern "C" fn no_subscribe(_: *mut c_void, _: u64, _: CaptureEventSink) -> Status {
    MEDIA_UNSUPPORTED
}
unsafe extern "C" fn no_unsubscribe(_: *mut c_void, _: u64) {}
unsafe extern "C" fn no_swap(_: *mut c_void, _: u64, _: u64, _: u32, _: BorrowedBytes) -> Status {
    MEDIA_UNSUPPORTED
}
#[test]
fn negotiated_resource_checks_capture_color_before_encoder() {
    let harness = Harness::default();
    let mut base = leased_capture_api(&harness);
    base.base.header = AbiHeader {
        size: CAPTURE_API_V1_8_SIZE,
        major: INTERFACE_MAJOR,
        minor: CAPTURE_RESOURCE_INTERFACE_MINOR,
    };
    let capture = CaptureApiV1_8 {
        base: CaptureApiV1_7 {
            base,
            subscribe_events: no_subscribe,
            unsubscribe_events: no_unsubscribe,
            resolve_swap: no_swap,
        },
        enumerate_resources: resource_enumerate,
        select_resource: resource_select,
        read_resource_frame: resource_read,
    };
    let mut config = configuration();
    config.gpu_processing = Some(
        serde_json::from_value(serde_json::json!({
            "service":"fixture.processor/1", "enabled":false,
            "source_hdr_required":true, "settings":{}
        }))
        .unwrap(),
    );
    let result = deliver_resource(
        &capture,
        &leased_encoder_api(&harness),
        &pump(&config, true, true),
        &mut std::collections::BTreeMap::new(),
    );
    assert_eq!(result.err(), Some("frame_rejected"));
    assert!(harness.submitted.borrow().is_empty());
    assert_eq!(harness.retained.get(), 0);
}

#[test]
fn negotiated_resource_reaches_encoder_and_retains_until_completion() {
    let harness = Harness::default();
    let mut base = leased_capture_api(&harness);
    base.base.header = AbiHeader {
        size: CAPTURE_API_V1_8_SIZE,
        major: INTERFACE_MAJOR,
        minor: CAPTURE_RESOURCE_INTERFACE_MINOR,
    };
    let capture = CaptureApiV1_8 {
        base: CaptureApiV1_7 {
            base,
            subscribe_events: no_subscribe,
            unsubscribe_events: no_unsubscribe,
            resolve_swap: no_swap,
        },
        enumerate_resources: resource_enumerate,
        select_resource: resource_select,
        read_resource_frame: resource_read,
    };
    let encoder = leased_encoder_api(&harness);
    let configuration = configuration();
    let mut cache = std::collections::BTreeMap::new();
    let result = deliver_resource(
        &capture,
        &encoder,
        &pump(&configuration, true, true),
        &mut cache,
    )
    .unwrap();
    assert!(result.submitted);
    assert_eq!(result.gpu_processor_us, None);
    assert_eq!(result.dmabuf_presentation, Some(44));
    assert_eq!(
        harness.submitted.borrow()[0],
        SubmittedFrame {
            inside_capture_callback: true,
            fd: 7,
            sequence: 9,
            timestamp_us: 1234,
            force_idr: 1
        }
    );
    assert_eq!(harness.retained.get(), 1);
    finish_encoder(&harness);
    assert_eq!(harness.retained.get(), 0);
    assert_eq!(harness.released.get(), 1);
}

#[test]
fn native_plane_selects_exact_registered_encoder_table() {
    use crate::plane::{NativePlane, Plane};
    use amaterasu_module_sdk::RuntimeClient;
    use std::sync::{Arc, Mutex};
    let harness = Harness::default();
    let capture = leased_capture_api(&harness);
    let builtin = leased_encoder_api(&harness);
    let external = leased_encoder_api(&harness);
    let fixture = RuntimeFixture {
        harness: &harness,
        capture: std::ptr::from_ref(&capture) as usize,
        encoder: std::ptr::from_ref(&builtin) as usize,
        external_encoder: std::ptr::from_ref(&external) as usize,
        processor: 0,
    };
    // SAFETY: the runtime endpoint and all tables outlive the plane.
    let runtime = unsafe {
        RuntimeClient::from_endpoint(HostApi {
            header: AbiHeader::of::<HostApi>(),
            context: std::ptr::from_ref(&fixture).cast_mut().cast(),
            invoke: invoke_lifecycle_runtime,
        })
    };
    let mut plane = NativePlane::bind(
        &runtime,
        Arc::new(Mutex::new(crate::queue::Queues::default())),
        &[CaptureProvider::Vkms].into(),
        &[("vendor".to_owned(), "example.encode/1".to_owned())].into(),
        &std::collections::BTreeMap::new(),
        &std::collections::BTreeMap::new(),
        None,
        None,
    )
    .unwrap();
    plane.select_encoder("example.encode/1");
    // SAFETY: both bindings and their v1 prefixes are alive for this test.
    let selected = unsafe { plane.encoders[&plane.active_encoder].table::<EncoderApi>() }.unwrap();
    assert_eq!(
        std::ptr::from_ref(selected) as usize,
        std::ptr::from_ref(&external) as usize
    );
    plane.select_encoder(super::super::ENCODE);
    let selected = unsafe { plane.encoders[&plane.active_encoder].table::<EncoderApi>() }.unwrap();
    assert_eq!(
        std::ptr::from_ref(selected) as usize,
        std::ptr::from_ref(&builtin) as usize
    );
}

#[test]
fn vkms_resource_color_metadata_reaches_the_encoder() {
    for (color, primaries, transfer) in [
        (
            DMABUF_COLOR_SRGB,
            FRAME_PRIMARIES_BT709,
            FRAME_TRANSFER_SRGB,
        ),
        (
            DMABUF_COLOR_BT2020_PQ,
            FRAME_PRIMARIES_BT2020,
            FRAME_TRANSFER_PQ,
        ),
    ] {
        let harness = Harness::default();
        harness.frame_color.set(color);
        harness.resource_color.set(FrameColorMetadata {
            version: 1,
            size: std::mem::size_of::<FrameColorMetadata>() as u32,
            primaries,
            transfer,
            matrix: FRAME_MATRIX_RGB,
            range: FRAME_RANGE_FULL,
        });
        let mut base = leased_capture_api(&harness);
        base.base.header = AbiHeader {
            size: CAPTURE_API_V1_8_SIZE,
            major: INTERFACE_MAJOR,
            minor: CAPTURE_RESOURCE_INTERFACE_MINOR,
        };
        let capture = CaptureApiV1_8 {
            base: CaptureApiV1_7 {
                base,
                subscribe_events: no_subscribe,
                unsubscribe_events: no_unsubscribe,
                resolve_swap: no_swap,
            },
            enumerate_resources: resource_enumerate,
            select_resource: resource_select,
            read_resource_frame: resource_read,
        };
        let encoder = leased_encoder_api(&harness);
        let config = configuration();
        let result = deliver_resource(
            &capture,
            &encoder,
            &pump(&config, true, false),
            &mut std::collections::BTreeMap::new(),
        )
        .expect("valid VKMS color metadata must not close the video stream");
        assert!(result.submitted);
        assert_eq!(harness.submitted.borrow().len(), 1);
        finish_encoder(&harness);
        assert_eq!(harness.retained.get(), 0);
    }
}

#[test]
fn negotiated_resource_survives_temporary_incompatible_geometry() {
    unsafe extern "C" fn transition_read(
        context: *mut c_void,
        stream: u64,
        sink: CaptureResourceFrameSink,
    ) -> Status {
        // The old selection is invalidated when the compositor changes geometry.
        let harness = unsafe { &*context.cast::<Harness>() };
        if harness.frame_width.get() != 16 {
            return MEDIA_UNSUPPORTED;
        }
        unsafe { resource_read(context, stream, sink) }
    }
    let harness = Harness::default();
    let mut base = leased_capture_api(&harness);
    base.base.header = AbiHeader {
        size: CAPTURE_API_V1_8_SIZE,
        major: INTERFACE_MAJOR,
        minor: CAPTURE_RESOURCE_INTERFACE_MINOR,
    };
    let capture = CaptureApiV1_8 {
        base: CaptureApiV1_7 {
            base,
            subscribe_events: no_subscribe,
            unsubscribe_events: no_unsubscribe,
            resolve_swap: no_swap,
        },
        enumerate_resources: resource_enumerate,
        select_resource: resource_select,
        read_resource_frame: transition_read,
    };
    let encoder = leased_encoder_api(&harness);
    let config = configuration();
    let mut cache = std::collections::BTreeMap::new();
    assert!(
        deliver_resource(&capture, &encoder, &pump(&config, true, false), &mut cache)
            .unwrap()
            .submitted
    );
    finish_encoder(&harness);
    harness.frame_width.set(32);
    for _ in 0..3 {
        let progress =
            deliver_resource(&capture, &encoder, &pump(&config, true, false), &mut cache).unwrap();
        assert!(progress.capture_deferred);
        assert!(progress.dmabuf_fallback.is_none());
        assert!(!progress.submitted);
    }
    assert_eq!(harness.submitted.borrow().len(), 1);
    assert_eq!(harness.retained.get(), 0);
    harness.frame_width.set(16);
    assert!(
        deliver_resource(&capture, &encoder, &pump(&config, true, false), &mut cache)
            .unwrap()
            .submitted
    );
    finish_encoder(&harness);
    assert_eq!(harness.submitted.borrow().len(), 2);
    assert_eq!(harness.retained.get(), 0);
    cache.clear();
    harness.frame_width.set(32);
    let initial =
        deliver_resource(&capture, &encoder, &pump(&config, true, false), &mut cache).unwrap();
    assert_eq!(
        initial.dmabuf_fallback,
        Some("capture_resource_no_dmabuf_offer")
    );
}
