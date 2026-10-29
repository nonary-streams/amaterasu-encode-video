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
    retained: Cell<usize>,
    released: Cell<usize>,
    retain_status: Cell<Status>,
    pending_lease: RefCell<Option<DmaBufFrameLease>>,
    processor_mode: Cell<u8>,
    processor_lease: RefCell<Option<DmaBufFrameLease>>,
    lifecycle_events: RefCell<Vec<&'static str>>,
    source_binding_alive: Cell<bool>,
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
            retained: Cell::new(0),
            released: Cell::new(0),
            retain_status: Cell::new(STATUS_OK),
            pending_lease: RefCell::new(None),
            processor_mode: Cell::new(0),
            processor_lease: RefCell::new(None),
            lifecycle_events: RefCell::new(Vec::new()),
            source_binding_alive: Cell::new(true),
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

unsafe extern "C" fn cancel_processor_leases(context: *mut c_void, _: u64) {
    // SAFETY: the fixture remains alive through the explicit cancellation barrier.
    let harness = unsafe { &*context.cast::<Harness>() };
    assert!(harness.source_binding_alive.get());
    harness
        .lifecycle_events
        .borrow_mut()
        .push("processor_cancel");
    if let Some(lease) = harness.processor_lease.borrow_mut().take() {
        // SAFETY: the processor owns this reference; its simulated reads are done.
        unsafe { lease.release.unwrap()(lease.context, -1) };
    }
}

fn finish_encoder(harness: &Harness) {
    let held = harness.pending_lease.borrow_mut().take().unwrap();
    // SAFETY: the encoder owns one retained reference and completes its reads.
    unsafe { held.release.unwrap()(held.context, -1) };
}

unsafe extern "C" fn process_lease(
    context: *mut c_void,
    _: u64,
    input: *const DmaBufFrameLease,
    freshness: u32,
    sink: DmaBufLeaseSink,
) -> Status {
    // SAFETY: test context and input remain live throughout this function.
    let harness = unsafe { &*context.cast::<Harness>() };
    match harness.processor_mode.get() {
        0 => unsafe { (sink.write)(sink.context, input, freshness) },
        1 => MEDIA_BUSY,
        5 => {
            // SAFETY: input remains borrowed and its provider accepts a reference.
            let lease = unsafe { &*input };
            let status = unsafe { lease.retain.unwrap()(lease.context) };
            if status != STATUS_OK {
                return status;
            }
            *harness.processor_lease.borrow_mut() = Some(*lease);
            // The retained reference models private GPU work still pending
            // after process returns. Cancellation must retire it separately.
            unsafe { (sink.write)(sink.context, input, freshness) }
        }
        2 => STATUS_OK,
        3 => {
            unsafe { (sink.write)(sink.context, input, freshness) };
            unsafe { (sink.write)(sink.context, input, freshness) }
        }
        _ => {
            let mut altered = unsafe { *input };
            altered.frame.sequence += 1;
            unsafe { (sink.write)(sink.context, &altered, freshness) }
        }
    }
}

fn gpu_processor_api(harness: &Harness) -> GpuProcessorApi {
    GpuProcessorApi {
        base: interface(
            GPU_PROCESSOR_API_V1_0_SIZE,
            GPU_PROCESSOR_INTERFACE_MINOR,
            harness,
        ),
        process: process_lease,
        cancel_dmabuf_leases: cancel_processor_leases,
    }
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
        None,
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
        1,
        "temporary video reference released exactly once"
    );
    let held = *harness.pending_lease.borrow();
    assert_eq!(held.unwrap().frame.sequence, 9);
    assert_eq!(held.unwrap().frame.timestamp_us, 1234);
    assert!(!harness.in_capture_callback.get());
    finish_encoder(&harness);
    assert_eq!(harness.retained.get(), 0);
    assert_eq!(harness.released.get(), 2);
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
            None,
            &pump(&config, true, false),
            false,
        );
        assert_eq!(harness.retained.get(), 0);
        assert_eq!(harness.released.get(), 1);
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
        None,
        &pump(&config, true, false),
        false,
    )
    .unwrap();
    assert!(result.dropped);
    assert!(harness.submitted.borrow().is_empty());
    assert_eq!(harness.retained.get(), 0);
    assert_eq!(harness.released.get(), 0);
}

#[test]
fn gpu_processor_preserves_ownership_and_rejects_invalid_output_protocols() {
    let harness = Harness::default();
    let config = configuration();
    let processor = gpu_processor_api(&harness);
    let result = deliver_leased(
        &leased_capture_api(&harness),
        &leased_encoder_api(&harness),
        Some(&processor),
        &pump(&config, true, false),
        false,
    )
    .unwrap();
    assert!(result.submitted);
    assert_eq!(harness.retained.get(), 1);
    assert_eq!(
        harness.released.get(),
        2,
        "input and processor output temporary refs released"
    );
    finish_encoder(&harness);
    assert_eq!(harness.retained.get(), 0);
    for mode in [1, 2, 3, 4] {
        let harness = Harness::default();
        harness.processor_mode.set(mode);
        let processor = gpu_processor_api(&harness);
        let result = deliver_leased(
            &leased_capture_api(&harness),
            &leased_encoder_api(&harness),
            Some(&processor),
            &pump(&config, true, false),
            false,
        );
        if mode == 1 {
            assert!(result.unwrap().dropped);
        } else {
            assert_eq!(result.err(), Some("frame_rejected"));
        }
        if harness.pending_lease.borrow().is_some() {
            finish_encoder(&harness);
        }
        assert_eq!(harness.retained.get(), 0);
    }
}

#[test]
fn optional_leases_preserve_legacy_compatibility_but_processors_require_ownership() {
    let capture_old = AbiHeader {
        size: CAPTURE_API_V1_4_SIZE,
        major: 1,
        minor: CAPTURE_DMABUF_INTERFACE_MINOR,
    };
    let capture_new = AbiHeader {
        size: CAPTURE_API_V1_6_SIZE,
        major: 1,
        minor: CAPTURE_LEASE_INTERFACE_MINOR,
    };
    let encoder_old = AbiHeader {
        size: ENCODER_API_V1_1_SIZE,
        major: 1,
        minor: ENCODER_DMABUF_INTERFACE_MINOR,
    };
    let encoder_new = AbiHeader {
        size: ENCODER_API_V1_2_SIZE,
        major: 1,
        minor: ENCODER_LEASE_INTERFACE_MINOR,
    };
    assert_eq!(use_lease_path(capture_new, encoder_old, false), Ok(false));
    assert_eq!(use_lease_path(capture_old, encoder_new, false), Ok(false));
    assert_eq!(use_lease_path(capture_new, encoder_new, false), Ok(true));
    assert_eq!(use_lease_path(capture_new, encoder_new, true), Ok(true));
    assert_eq!(
        use_lease_path(capture_old, encoder_new, true),
        Err("gpu_processor_requires_frame_lease")
    );
    assert_eq!(
        use_lease_path(capture_new, encoder_old, true),
        Err("gpu_processor_requires_encoder_lease")
    );

    let harness = Harness::default();
    harness.deliver_freshness.set(None);
    harness.capture_status.set(MEDIA_UNSUPPORTED);
    let config = configuration();
    let result = deliver_leased(
        &leased_capture_api(&harness),
        &leased_encoder_api(&harness),
        None,
        &pump(&config, true, false),
        false,
    )
    .unwrap();
    assert_eq!(
        result.dmabuf_fallback,
        Some("capture_dmabuf_lease_unsupported")
    );
    assert_eq!(harness.retained.get(), 0);
    assert!(harness.submitted.borrow().is_empty());
}

#[test]
fn gpu_processor_receives_cached_reuse_freshness_without_reclassifying_it() {
    let harness = Harness::default();
    harness
        .deliver_freshness
        .set(Some(CAPTURE_FRAME_CACHED_REUSE));
    let config = configuration();
    let processor = gpu_processor_api(&harness);
    let result = deliver_leased(
        &leased_capture_api(&harness),
        &leased_encoder_api(&harness),
        Some(&processor),
        &pump(&config, false, false),
        true,
    )
    .unwrap();
    assert!(result.submitted);
    assert!(result.capture_reused);
    assert_eq!(result.dmabuf_fresh, Some(false));
    finish_encoder(&harness);
    assert_eq!(harness.retained.get(), 0);
}

struct RuntimeFixture<'a> {
    harness: &'a Harness,
    capture: usize,
    encoder: usize,
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
        Command::NativeBind { service, .. } => {
            let (binding, address) = match service.as_str() {
                super::super::ENCODE => (1, fixture.encoder),
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

#[test]
fn native_plane_drop_retires_encoder_and_processor_before_releasing_source_binding() {
    use crate::plane::{NativePlane, Plane};
    use amaterasu_module_sdk::RuntimeClient;
    use std::sync::{Arc, Mutex};

    // Exercise accepted input and processor work left pending after encoder busy.
    for encoder_status in [STATUS_OK, MEDIA_BUSY] {
        let harness = Harness::default();
        harness.processor_mode.set(5);
        harness.encoder_status.set(encoder_status);
        let capture = leased_capture_api(&harness);
        let encoder = leased_encoder_api(&harness);
        let processor = gpu_processor_api(&harness);
        let fixture = RuntimeFixture {
            harness: &harness,
            capture: std::ptr::from_ref(&capture) as usize,
            encoder: std::ptr::from_ref(&encoder) as usize,
            processor: std::ptr::from_ref(&processor) as usize,
        };
        // SAFETY: fixture, runtime endpoint and table allocations outlive the plane.
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
            None,
            Some("fixture.gpu-processor/1"),
        )
        .unwrap();
        let config = configuration();
        let progress = plane.pump(pump(&config, true, false)).unwrap();
        assert_eq!(progress.submitted, encoder_status == STATUS_OK);
        assert_eq!(
            harness.retained.get(),
            if encoder_status == STATUS_OK { 2 } else { 1 }
        );
        drop(plane);
        assert_eq!(harness.retained.get(), 0);
        assert_eq!(
            *harness.lifecycle_events.borrow(),
            [
                "encoder_cancel",
                "processor_cancel",
                "encoder_release",
                "processor_release",
                "capture_release",
            ]
        );
    }
}

#[test]
fn completed_stream_retires_leases_once_without_growing_shutdown_history() {
    use crate::plane::{NativePlane, Plane};
    use amaterasu_module_sdk::RuntimeClient;
    use std::sync::{Arc, Mutex};
    let harness = Harness::default();
    harness.processor_mode.set(5);
    let capture = leased_capture_api(&harness);
    let encoder = leased_encoder_api(&harness);
    let processor = gpu_processor_api(&harness);
    let fixture = RuntimeFixture {
        harness: &harness,
        capture: std::ptr::from_ref(&capture) as usize,
        encoder: std::ptr::from_ref(&encoder) as usize,
        processor: std::ptr::from_ref(&processor) as usize,
    };
    // SAFETY: all native tables and fixture state outlive the plane's bindings.
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
        None,
        Some("fixture.gpu-processor/1"),
    )
    .unwrap();
    let config = configuration();
    assert!(plane.pump(pump(&config, true, false)).unwrap().submitted);
    plane.finish_stream(1);
    assert_eq!(harness.retained.get(), 0);
    plane.finish_stream(1);
    drop(plane);
    assert_eq!(
        *harness.lifecycle_events.borrow(),
        [
            "encoder_cancel",
            "processor_cancel",
            "encoder_release",
            "processor_release",
            "capture_release",
        ]
    );
}
