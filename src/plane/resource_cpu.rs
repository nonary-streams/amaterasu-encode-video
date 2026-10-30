//! Bridge a negotiated standard CPU resource into the synchronous legacy encoder.
use super::*;

const MAX_OFFERS: usize = 64;

#[derive(Clone, Copy)]
struct Selection {
    id: u64,
    caps: CpuResourceCapabilities,
}

struct Offers {
    ids: Vec<u64>,
    selected: Option<Selection>,
    width: u32,
    height: u32,
    invalid: bool,
}

fn none(descriptor: &FrameDescriptor) -> bool {
    descriptor.kind == FRAME_SYNC_NONE
        && descriptor.version == 0
        && descriptor.size == 0
        && descriptor.reserved == 0
        && descriptor.namespace == [0; 16]
        && descriptor.data.is_null()
}

// The legacy CPU submission does not carry source color metadata. Accept only
// unspecified color until that interface can validate or convert it.
fn unspecified_color(color: &FrameColorMetadata) -> bool {
    let no_components =
        color.primaries == 0 && color.transfer == 0 && color.matrix == 0 && color.range == 0;
    no_components
        && ((color.version == 0 && color.size == 0)
            || (color.version == 1 && color.size == size_of::<FrameColorMetadata>() as u32))
}

// This bridge does not use provider timestamps for pacing or compare clock
// domains. Validate both envelopes independently without inferring an epoch.
fn valid_clock(clock: &FrameClockMetadata) -> bool {
    if clock.version == 0 {
        return clock.size == 0
            && clock.domain == 0
            && clock.reserved == 0
            && clock.epoch_id == [0; 16];
    }
    clock.version == 1
        && clock.size == size_of::<FrameClockMetadata>() as u32
        && clock.reserved == 0
        && matches!(
            clock.domain,
            FRAME_CLOCK_UNKNOWN | FRAME_CLOCK_STREAM_RELATIVE | FRAME_CLOCK_HOST_MONOTONIC
        )
}

unsafe extern "C" fn offer(context: *mut c_void, offer: *const CaptureResourceOffer) -> Status {
    if context.is_null() || offer.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: the caller supplies the stack context and offer for this callback.
    let state = unsafe { &mut *context.cast::<Offers>() };
    let offer = unsafe { &*offer };
    if state.ids.len() >= MAX_OFFERS
        || offer.id == 0
        || state.ids.contains(&offer.id)
        || offer.reserved != 0
    {
        state.invalid = true;
        return STATUS_INVALID_ARGUMENT;
    }
    state.ids.push(offer.id);
    let resource = &offer.resource;
    if state.selected.is_some()
        || offer.accessibility & FRAME_ACCESS_CPU == 0
        || offer.accessibility & !(FRAME_ACCESS_CPU | FRAME_ACCESS_GPU) != 0
        || !matches!(
            offer.ownership,
            FRAME_OWNERSHIP_BORROWED | FRAME_OWNERSHIP_RETAINABLE
        )
        || resource.kind != FRAME_RESOURCE_CPU
        || resource.version != 1
        || resource.reserved != 0
        || resource.namespace != [0; 16]
        || resource.size != size_of::<CpuResourceCapabilities>() as u32
        || resource.data.is_null()
        || !none(&offer.acquire_sync)
        || !none(&offer.completion_sync)
    {
        return STATUS_OK;
    }
    // SAFETY: standard CPU offer schema supplies a readable capabilities object.
    let caps = unsafe {
        resource
            .data
            .cast::<CpuResourceCapabilities>()
            .read_unaligned()
    };
    if matches!(caps.format, PIXEL_BGRA8 | PIXEL_NV12)
        && caps.min_width > 0
        && caps.min_height > 0
        && caps.min_width <= state.width
        && state.width <= caps.max_width
        && caps.min_height <= state.height
        && state.height <= caps.max_height
        && frame_size(
            state.width,
            state.height,
            caps.format,
            if caps.format == PIXEL_BGRA8 {
                state.width.saturating_mul(4)
            } else {
                state.width
            },
        )
        .is_ok()
    {
        state.selected = Some(Selection { id: offer.id, caps });
    }
    STATUS_OK
}

struct Delivery<'a, 'b> {
    submission: &'a mut Submission<'b>,
    selection: Selection,
    generation: u64,
    calls: u32,
    invalid: bool,
    callback_status: Option<Status>,
}

unsafe extern "C" fn frame(context: *mut c_void, frame: *const CaptureResourceFrame) -> Status {
    if context.is_null() || frame.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: provider lends the frame and stack context for this synchronous call.
    let state = unsafe { &mut *context.cast::<Delivery<'_, '_>>() };
    let frame = unsafe { &*frame };
    state.calls += 1;
    let resource = &frame.resource;
    if state.calls != 1
        || frame.offer_generation != state.generation
        || frame.offer_id != state.selection.id
        || frame.reserved != 0
        || !unspecified_color(&frame.color)
        || !valid_clock(&frame.clock)
        || !valid_clock(&frame.pacing_clock)
        || !matches!(
            frame.freshness,
            CAPTURE_FRAME_FRESH | CAPTURE_FRAME_CACHED_REUSE
        )
        || resource.kind != FRAME_RESOURCE_CPU
        || resource.version != 1
        || resource.reserved != 0
        || resource.namespace != [0; 16]
        || resource.size < size_of::<CpuFrame>() as u32
        || resource.data.is_null()
        || !none(&frame.acquire_sync)
    {
        state.invalid = true;
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: selected CPU v1 frame descriptor points to a readable CpuFrame.
    let cpu = unsafe { resource.data.cast::<CpuFrame>().read_unaligned() };
    let caps = state.selection.caps;
    if cpu.width != frame.width
        || cpu.height != frame.height
        || cpu.timestamp_us != frame.timestamp_us
        || cpu.sequence != frame.sequence
        || cpu.format != caps.format
        || cpu.width < caps.min_width
        || cpu.width > caps.max_width
        || cpu.height < caps.min_height
        || cpu.height > caps.max_height
    {
        state.invalid = true;
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: the provider lends the pixels through this synchronous callback.
    let status = unsafe {
        submit_frame(
            state.submission,
            &cpu,
            Some(frame.freshness == CAPTURE_FRAME_FRESH),
            Some(frame.capture_pacing_timestamp_us),
        )
    };
    state.callback_status = Some(status);
    status
}

#[derive(Clone, Copy)]
pub(super) struct CachedSelection {
    provider: CaptureProvider,
    stream: u64,
    generation: u64,
    selection: Selection,
}

enum Negotiation {
    Selected(Box<CachedSelection>),
    Busy,
    Stale,
}

fn enumeration_status(status: Status) -> Result<bool, &'static str> {
    match status {
        STATUS_OK => Ok(true),
        MEDIA_BUSY | MEDIA_PENDING => Ok(false),
        _ => Err("capture_resource_enumeration_failed"),
    }
}

fn selection_status(status: Status) -> Result<Option<bool>, &'static str> {
    match status {
        STATUS_OK => Ok(Some(true)),
        MEDIA_UNSUPPORTED => Ok(Some(false)),
        MEDIA_BUSY | MEDIA_PENDING => Ok(None),
        _ => Err("capture_resource_selection_failed"),
    }
}

fn negotiate(
    api: &CaptureApiV1_8,
    stream: u64,
    submission: &Submission<'_>,
    provider: CaptureProvider,
) -> Result<Negotiation, &'static str> {
    let mut offers = Offers {
        ids: Vec::new(),
        selected: None,
        width: submission.configuration.width,
        height: submission.configuration.height,
        invalid: false,
    };
    let mut generation = 0;
    let sink = CaptureResourceOfferSink {
        context: (&mut offers as *mut Offers).cast(),
        write: offer,
    };
    // SAFETY: callbacks are synchronous and the validated binding remains live.
    let status = unsafe {
        (api.enumerate_resources)(api.base.base.base.context, stream, sink, &mut generation)
    };
    if !enumeration_status(status)? {
        return Ok(Negotiation::Busy);
    }
    if offers.invalid || generation == 0 {
        return Err("capture_resource_enumeration_failed");
    }
    let selection = offers.selected.ok_or("capture_resource_no_cpu_offer")?;
    let status = unsafe {
        (api.select_resource)(api.base.base.base.context, stream, generation, selection.id)
    };
    match selection_status(status)? {
        None => Ok(Negotiation::Busy),
        Some(false) => Ok(Negotiation::Stale),
        Some(true) => Ok(Negotiation::Selected(Box::new(CachedSelection {
            provider,
            stream,
            generation,
            selection,
        }))),
    }
}

pub(super) fn read(
    api: &CaptureApiV1_8,
    stream: u64,
    provider: CaptureProvider,
    cache: &mut BTreeMap<u64, CachedSelection>,
    submission: &mut Submission<'_>,
) -> Result<Status, &'static str> {
    let video = submission.video;
    if cache
        .get(&video)
        .is_some_and(|entry| entry.stream != stream || entry.provider != provider)
    {
        cache.remove(&video);
    }
    // Retry once after a stale generation. Repeated invalidation is an error,
    // rather than silently leaving the video stream pending forever.
    for _ in 0..2 {
        let selected = match cache.get(&video).copied() {
            Some(entry) => entry,
            None => match negotiate(api, stream, submission, provider)? {
                Negotiation::Selected(entry) => {
                    cache.insert(video, *entry);
                    *entry
                }
                Negotiation::Busy => return Ok(MEDIA_BUSY),
                Negotiation::Stale => continue,
            },
        };
        let mut delivery = Delivery {
            submission,
            selection: selected.selection,
            generation: selected.generation,
            calls: 0,
            invalid: false,
            callback_status: None,
        };
        let sink = CaptureResourceFrameSink {
            context: (&mut delivery as *mut Delivery<'_, '_>).cast(),
            write: frame,
        };
        // SAFETY: sink, submission and provider are live for the synchronous call.
        let status = unsafe { (api.read_resource_frame)(api.base.base.base.context, stream, sink) };
        if delivery.invalid
            || (delivery.calls == 0 && status == STATUS_OK)
            || (delivery.calls == 1 && delivery.callback_status != Some(status))
            || delivery.calls > 1
        {
            return Err("capture_resource_protocol");
        }
        if status == MEDIA_UNSUPPORTED {
            cache.remove(&video);
            continue;
        }
        return Ok(status);
    }
    Err("capture_resource_stale")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    fn none_descriptor() -> FrameDescriptor {
        FrameDescriptor {
            kind: FRAME_SYNC_NONE,
            version: 0,
            size: 0,
            reserved: 0,
            namespace: [0; 16],
            data: ptr::null(),
        }
    }

    fn cpu_offer(caps: &CpuResourceCapabilities) -> CaptureResourceOffer {
        CaptureResourceOffer {
            id: 7,
            ownership: FRAME_OWNERSHIP_RETAINABLE,
            accessibility: FRAME_ACCESS_CPU,
            reserved: 0,
            resource: FrameDescriptor {
                kind: FRAME_RESOURCE_CPU,
                version: 1,
                size: size_of::<CpuResourceCapabilities>() as u32,
                reserved: 0,
                namespace: [0; 16],
                data: (caps as *const CpuResourceCapabilities).cast(),
            },
            acquire_sync: none_descriptor(),
            completion_sync: none_descriptor(),
        }
    }

    fn offers() -> Offers {
        Offers {
            ids: vec![],
            selected: None,
            width: 64,
            height: 48,
            invalid: false,
        }
    }

    unsafe extern "C" fn busy_encoder(
        _: *mut c_void,
        _: u64,
        _: *const CpuFrame,
        _: u32,
    ) -> Status {
        MEDIA_BUSY
    }
    unsafe extern "C" fn receive(_: *mut c_void, _: u64, _: PacketSink) -> Status {
        MEDIA_PENDING
    }
    unsafe extern "C" fn drain(_: *mut c_void, _: u64) -> Status {
        STATUS_OK
    }
    unsafe extern "C" fn retain(_: *mut c_void) -> Status {
        MEDIA_UNSUPPORTED
    }
    unsafe extern "C" fn release(_: *mut c_void, _: *const FrameDescriptor) {}

    #[test]
    fn cpu_callback_preserves_encoder_backpressure_without_failing_capture() {
        let api = EncoderApi {
            base: NativeInterface {
                header: AbiHeader::of::<EncoderApi>(),
                context: ptr::null_mut(),
                release: None,
            },
            submit: busy_encoder,
            receive,
            drain,
        };
        let configuration: Configuration = serde_json::from_value(
            serde_json::json!({"width":64,"height":48,"fps":60,"bitrate":100000}),
        )
        .unwrap();
        let mut submission = Submission {
            api: &api,
            processor: None,
            video: 1,
            encoder: 2,
            configuration: &configuration,
            status: MEDIA_PENDING,
            called: false,
            allow_resize: false,
            track_wgc_arrival: false,
            capture_reused: false,
            capture_arrival: None,
            force_idr: false,
            timestamp_us: 3,
            sequence: 4,
            frame: None,
            owned_frame: None,
            encoder_submit_us: 0,
            frame_copy_us: 0,
        };
        let bytes = vec![0u8; 64 * 48 * 3 / 2];
        let cpu = CpuFrame {
            width: 64,
            height: 48,
            format: PIXEL_NV12,
            stride: 64,
            data: bytes.as_ptr(),
            len: bytes.len(),
            timestamp_us: 11,
            sequence: 12,
        };
        let caps = CpuResourceCapabilities {
            format: PIXEL_NV12,
            min_width: 64,
            max_width: 64,
            min_height: 48,
            max_height: 48,
        };
        let mut state = Delivery {
            submission: &mut submission,
            selection: Selection { id: 7, caps },
            generation: 5,
            calls: 0,
            invalid: false,
            callback_status: None,
        };
        let resource_frame = CaptureResourceFrame {
            offer_generation: 5,
            offer_id: 7,
            timestamp_us: 11,
            capture_pacing_timestamp_us: 11,
            sequence: 12,
            width: 64,
            height: 48,
            freshness: CAPTURE_FRAME_FRESH,
            reserved: 0,
            color: FrameColorMetadata::default(),
            clock: FrameClockMetadata::default(),
            pacing_clock: FrameClockMetadata::default(),
            resource: FrameDescriptor {
                kind: FRAME_RESOURCE_CPU,
                version: 1,
                size: size_of::<CpuFrame>() as u32,
                reserved: 0,
                namespace: [0; 16],
                data: (&cpu as *const CpuFrame).cast(),
            },
            acquire_sync: none_descriptor(),
            context: ptr::null_mut(),
            retain,
            release,
        };
        // The sink acknowledges delivery while the established CPU bridge records
        // encoder BUSY as a dropped frame, without failing the capture provider.
        assert_eq!(
            unsafe {
                frame(
                    (&mut state as *mut Delivery<'_, '_>).cast(),
                    &resource_frame,
                )
            },
            STATUS_OK
        );
        assert_eq!(state.callback_status, Some(STATUS_OK));
        assert_eq!(state.submission.status, MEDIA_BUSY);
        assert!(state.submission.called);
    }

    #[test]
    fn color_and_clock_metadata_are_not_silently_interpreted() {
        assert!(unspecified_color(&FrameColorMetadata::default()));
        let mut color = FrameColorMetadata {
            version: 1,
            size: size_of::<FrameColorMetadata>() as u32,
            ..Default::default()
        };
        assert!(unspecified_color(&color));
        color.transfer = FRAME_TRANSFER_PQ;
        assert!(!unspecified_color(&color));
        color.transfer = 0;
        color.version = 2;
        assert!(!unspecified_color(&color));
        assert!(valid_clock(&FrameClockMetadata::default()));
        let mut clock = FrameClockMetadata {
            version: 1,
            size: size_of::<FrameClockMetadata>() as u32,
            domain: FRAME_CLOCK_HOST_MONOTONIC,
            ..Default::default()
        };
        assert!(valid_clock(&clock));
        clock.domain = u32::MAX;
        assert!(!valid_clock(&clock));
        clock.version = 0;
        assert!(!valid_clock(&clock));
    }

    #[test]
    fn negotiation_busy_defers_before_reading_outputs_or_caching_selection() {
        for status in [MEDIA_BUSY, MEDIA_PENDING] {
            assert_eq!(enumeration_status(status), Ok(false));
            assert_eq!(selection_status(status), Ok(None));
        }
        assert_eq!(enumeration_status(STATUS_OK), Ok(true));
        assert_eq!(selection_status(STATUS_OK), Ok(Some(true)));
        assert_eq!(selection_status(MEDIA_UNSUPPORTED), Ok(Some(false)));
    }

    #[test]
    fn only_compatible_standard_cpu_offer_is_selected() {
        let caps = CpuResourceCapabilities {
            format: PIXEL_NV12,
            min_width: 64,
            max_width: 64,
            min_height: 48,
            max_height: 48,
        };
        let mut state = offers();
        let mut candidate = cpu_offer(&caps);
        candidate.resource.kind = FRAME_RESOURCE_VENDOR_BASE;
        assert_eq!(
            unsafe { offer((&mut state as *mut Offers).cast(), &candidate) },
            STATUS_OK
        );
        assert!(state.selected.is_none());
        candidate.id = 8;
        candidate.resource.kind = FRAME_RESOURCE_CPU;
        candidate.acquire_sync.kind = FRAME_SYNC_SYNC_FILE;
        assert_eq!(
            unsafe { offer((&mut state as *mut Offers).cast(), &candidate) },
            STATUS_OK
        );
        assert!(state.selected.is_none());
        candidate.id = 9;
        candidate.acquire_sync = none_descriptor();
        assert_eq!(
            unsafe { offer((&mut state as *mut Offers).cast(), &candidate) },
            STATUS_OK
        );
        assert_eq!(state.selected.unwrap().id, 9);
    }

    #[test]
    fn rejects_duplicate_and_excess_offers() {
        let caps = CpuResourceCapabilities {
            format: PIXEL_NV12,
            min_width: 64,
            max_width: 64,
            min_height: 48,
            max_height: 48,
        };
        let mut state = offers();
        let candidate = cpu_offer(&caps);
        assert_eq!(
            unsafe { offer((&mut state as *mut Offers).cast(), &candidate) },
            STATUS_OK
        );
        assert_eq!(
            unsafe { offer((&mut state as *mut Offers).cast(), &candidate) },
            STATUS_INVALID_ARGUMENT
        );
        assert!(state.invalid);
        let mut state = offers();
        for id in 1..=MAX_OFFERS as u64 {
            let mut candidate = cpu_offer(&caps);
            candidate.id = id;
            assert_eq!(
                unsafe { offer((&mut state as *mut Offers).cast(), &candidate) },
                STATUS_OK
            );
        }
        let mut candidate = cpu_offer(&caps);
        candidate.id = MAX_OFFERS as u64 + 1;
        assert_eq!(
            unsafe { offer((&mut state as *mut Offers).cast(), &candidate) },
            STATUS_INVALID_ARGUMENT
        );
    }
}
