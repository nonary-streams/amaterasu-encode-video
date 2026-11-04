//! Adapt standard v1.8 resources to the existing immutable DMA-BUF lease ABI.
use super::*;
use crate::model::CaptureProvider;
use std::{cell::Cell, collections::BTreeMap, mem::size_of, ptr};

#[derive(Clone, Copy)]
struct Selection {
    id: u64,
    caps: DmaBufResourceCapabilities,
}
#[derive(Clone, Copy)]
pub(in crate::plane) struct CachedSelection {
    provider: CaptureProvider,
    stream: u64,
    generation: u64,
    selection: Selection,
    reconfigure_started: Option<Instant>,
}
const RECONFIGURE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn reconfiguration_expired(started: Option<Instant>, now: Instant) -> bool {
    started.is_some_and(|started| now.saturating_duration_since(started) >= RECONFIGURE_TIMEOUT)
}

struct Offers {
    ids: Vec<u64>,
    selection: Option<Selection>,
    width: u32,
    height: u32,
    invalid: bool,
}
fn sync_file_offer(d: &FrameDescriptor) -> bool {
    d.kind == FRAME_SYNC_SYNC_FILE
        && d.version == 1
        && d.size == 0
        && d.reserved == 0
        && d.namespace == [0; 16]
        && d.data.is_null()
}
unsafe extern "C" fn offer(context: *mut c_void, offer: *const CaptureResourceOffer) -> Status {
    if context.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: enumeration lends the stack state for this callback.
    let state = unsafe { &mut *context.cast::<Offers>() };
    if offer.is_null() {
        state.invalid = true;
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: a non-null offer is lent for this callback.
    let offer = unsafe { &*offer };
    if state.ids.len() >= 64
        || offer.id == 0
        || state.ids.contains(&offer.id)
        || offer.reserved != 0
    {
        state.invalid = true;
        return STATUS_INVALID_ARGUMENT;
    }
    state.ids.push(offer.id);
    let resource = &offer.resource;
    if state.selection.is_some()
        || offer.ownership != FRAME_OWNERSHIP_RETAINABLE
        || offer.accessibility & FRAME_ACCESS_GPU == 0
        || offer.accessibility & !(FRAME_ACCESS_CPU | FRAME_ACCESS_GPU) != 0
        || resource.kind != FRAME_RESOURCE_DMABUF
        || resource.version != 1
        || resource.reserved != 0
        || resource.namespace != [0; 16]
        || resource.size != size_of::<DmaBufResourceCapabilities>() as u32
        || resource.data.is_null()
        || !super::super::resource_cpu::none(&offer.acquire_sync)
        || !sync_file_offer(&offer.completion_sync)
    {
        return STATUS_OK;
    }
    // SAFETY: the standard offer declares a readable capabilities payload.
    let caps = unsafe {
        resource
            .data
            .cast::<DmaBufResourceCapabilities>()
            .read_unaligned()
    };
    if caps.fourcc != 0
        && matches!(caps.color, DMABUF_COLOR_SRGB | DMABUF_COLOR_BT2020_PQ)
        && caps.min_width > 0
        && caps.min_height > 0
        && (caps.min_width..=caps.max_width).contains(&state.width)
        && (caps.min_height..=caps.max_height).contains(&state.height)
    {
        state.selection = Some(Selection { id: offer.id, caps });
    }
    STATUS_OK
}

// One sentinel holds this allocation through synchronous submission. Every
// successful downstream retain acquires a provider reference and another count.
struct Adapter {
    frame: CaptureResourceFrame,
    references: Cell<usize>,
}
unsafe extern "C" fn retain(context: *mut c_void) -> Status {
    // SAFETY: the sentinel or a retained reference keeps the adapter alive.
    let adapter = unsafe { &*context.cast::<Adapter>() };
    let status = unsafe { (adapter.frame.retain)(adapter.frame.context) };
    if status == STATUS_OK {
        adapter.references.set(adapter.references.get() + 1);
    }
    status
}
unsafe fn drop_reference(context: *mut c_void) {
    let adapter = unsafe { &*context.cast::<Adapter>() };
    let remaining = adapter.references.get() - 1;
    adapter.references.set(remaining);
    if remaining == 0 {
        drop(unsafe { Box::from_raw(context.cast::<Adapter>()) });
    }
}
unsafe extern "C" fn release(context: *mut c_void, fd: i32) {
    let adapter = unsafe { &*context.cast::<Adapter>() };
    let sync = FrameSyncFile { fd };
    let descriptor = if fd == -1 {
        FrameDescriptor {
            kind: FRAME_SYNC_NONE,
            version: 0,
            size: 0,
            reserved: 0,
            namespace: [0; 16],
            data: ptr::null(),
        }
    } else {
        FrameDescriptor {
            kind: FRAME_SYNC_SYNC_FILE,
            version: 1,
            size: size_of::<FrameSyncFile>() as u32,
            reserved: 0,
            namespace: [0; 16],
            data: (&sync as *const FrameSyncFile).cast(),
        }
    };
    // SAFETY: completion is borrowed by the generic provider during release.
    unsafe { (adapter.frame.release)(adapter.frame.context, &descriptor) };
    // Legacy release transfers fd ownership; generic release only borrows it.
    #[cfg(unix)]
    if fd >= 0 {
        use std::os::fd::{FromRawFd, OwnedFd};
        drop(unsafe { OwnedFd::from_raw_fd(fd) });
    }
    unsafe { drop_reference(context) };
}
/// The generic envelope can describe the same color space already carried by
/// the DMA-BUF. Absence and individually unknown components are compatible;
/// explicit metadata must agree, because this adapter does no color conversion.
fn color_matches_resource(color: &FrameColorMetadata, resource_color: u32) -> bool {
    if super::super::resource_cpu::unspecified_color(color) {
        return true;
    }
    if color.version != 1 || color.size != size_of::<FrameColorMetadata>() as u32 {
        return false;
    }
    let (primaries, transfer) = match resource_color {
        DMABUF_COLOR_SRGB => (FRAME_PRIMARIES_BT709, FRAME_TRANSFER_SRGB),
        DMABUF_COLOR_BT2020_PQ => (FRAME_PRIMARIES_BT2020, FRAME_TRANSFER_PQ),
        _ => return false,
    };
    [
        (color.primaries, primaries),
        (color.transfer, transfer),
        (color.matrix, FRAME_MATRIX_RGB),
        (color.range, FRAME_RANGE_FULL),
    ]
    .into_iter()
    .all(|(actual, expected)| actual == FRAME_COLOR_UNKNOWN || actual == expected)
}

struct Delivery<'a> {
    common: LeasedSubmission<'a>,
    selected: CachedSelection,
    calls: u32,
    invalid: bool,
}
unsafe extern "C" fn frame(context: *mut c_void, frame: *const CaptureResourceFrame) -> Status {
    if context.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: read lends its stack state through the callback.
    let state = unsafe { &mut *context.cast::<Delivery<'_>>() };
    if frame.is_null() {
        state.invalid = true;
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: a non-null frame is lent for this callback.
    let frame = unsafe { &*frame };
    state.calls += 1;
    let r = &frame.resource;
    if state.calls != 1
        || frame.offer_generation != state.selected.generation
        || frame.offer_id != state.selected.selection.id
        || frame.reserved != 0
        || !super::super::resource_cpu::valid_clock(&frame.clock)
        || !super::super::resource_cpu::valid_clock(&frame.pacing_clock)
        || !matches!(
            frame.freshness,
            CAPTURE_FRAME_FRESH | CAPTURE_FRAME_CACHED_REUSE
        )
        || r.kind != FRAME_RESOURCE_DMABUF
        || r.version != 1
        || r.reserved != 0
        || r.namespace != [0; 16]
        || r.size < size_of::<DmaBufFrame>() as u32
        || r.data.is_null()
        || !super::super::resource_cpu::none(&frame.acquire_sync)
    {
        state.invalid = true;
        return STATUS_INVALID_ARGUMENT;
    }
    let gpu = unsafe { r.data.cast::<DmaBufFrame>().read_unaligned() };
    let caps = state.selected.selection.caps;
    if gpu.validate().is_err()
        || !color_matches_resource(&frame.color, gpu.color)
        || gpu.width != frame.width
        || gpu.height != frame.height
        || gpu.timestamp_us != frame.timestamp_us
        || gpu.sequence != frame.sequence
        || gpu.fourcc != caps.fourcc
        || gpu.modifier != caps.modifier
        || gpu.color != caps.color
        || !(caps.min_width..=caps.max_width).contains(&gpu.width)
        || !(caps.min_height..=caps.max_height).contains(&gpu.height)
    {
        state.invalid = true;
        return STATUS_INVALID_ARGUMENT;
    }
    let adapter = Box::into_raw(Box::new(Adapter {
        frame: *frame,
        references: Cell::new(1),
    }))
    .cast::<c_void>();
    let lease = DmaBufFrameLease {
        frame: gpu,
        context: adapter,
        retain: Some(retain),
        release: Some(release),
    };
    // SAFETY: the sentinel owns the adapter through this call; downstream
    // stages retain independently and existing cancellation keeps bindings live.
    let result = unsafe {
        submit_leased_frame(
            (&mut state.common as *mut LeasedSubmission<'_>).cast(),
            &lease,
            frame.freshness,
        )
    };
    unsafe { drop_reference(adapter) };
    result
}

pub(in crate::plane) fn deliver(
    api: &CaptureApiV1_8,
    encoder: &EncoderApiV1_2,
    processor: Option<&GpuProcessorApi>,
    input: &Pump<'_>,
    cache: &mut BTreeMap<u64, CachedSelection>,
) -> Result<Progress, &'static str> {
    if cache
        .get(&input.video)
        .is_some_and(|s| s.provider != input.provider || s.stream != input.capture)
    {
        cache.remove(&input.video);
    }
    // A compositor transition may invalidate the selected allocation before
    // a compatible replacement appears. Keep the stream and retry, but bound
    // the wait so a permanently incompatible source still reports failure.
    if reconfiguration_expired(
        cache.get(&input.video).and_then(|s| s.reconfigure_started),
        Instant::now(),
    ) {
        return Err("capture_resource_reconfiguration_timeout");
    }
    for _ in 0..2 {
        let selected = if let Some(s) = cache
            .get(&input.video)
            .copied()
            .filter(|s| s.generation != 0)
        {
            s
        } else {
            let mut offers = Offers {
                ids: vec![],
                selection: None,
                width: input.configuration.width,
                height: input.configuration.height,
                invalid: false,
            };
            let mut generation = 0;
            let sink = CaptureResourceOfferSink {
                context: (&mut offers as *mut Offers).cast(),
                write: offer,
            };
            let status = unsafe {
                (api.enumerate_resources)(
                    api.base.base.base.context,
                    input.capture,
                    sink,
                    &mut generation,
                )
            };
            match status {
                MEDIA_BUSY | MEDIA_PENDING => return Ok(Progress::default()),
                STATUS_OK => {}
                _ => return Err("capture_resource_enumeration_failed"),
            }
            if offers.invalid || generation == 0 {
                return Err("capture_resource_enumeration_failed");
            }
            let Some(selection) = offers.selection else {
                if cache.contains_key(&input.video) {
                    return Ok(Progress {
                        capture_deferred: true,
                        ..Progress::default()
                    });
                }
                return Ok(Progress {
                    dmabuf_fallback: Some("capture_resource_no_dmabuf_offer"),
                    ..Progress::default()
                });
            };
            match unsafe {
                (api.select_resource)(
                    api.base.base.base.context,
                    input.capture,
                    generation,
                    selection.id,
                )
            } {
                STATUS_OK => {}
                MEDIA_BUSY | MEDIA_PENDING => return Ok(Progress::default()),
                MEDIA_UNSUPPORTED => continue,
                _ => return Err("capture_resource_selection_failed"),
            }
            let s = CachedSelection {
                provider: input.provider,
                stream: input.capture,
                generation,
                selection,
                reconfigure_started: cache.get(&input.video).and_then(|s| s.reconfigure_started),
            };
            cache.insert(input.video, s);
            s
        };
        let legacy = unsafe { &*(encoder as *const EncoderApiV1_2).cast::<EncoderApiV1_1>() };
        let mut delivery = Delivery {
            common: LeasedSubmission {
                common: Submission {
                    api: legacy,
                    encoder: input.encoder,
                    width: input.configuration.width,
                    height: input.configuration.height,
                    force_idr: input.force_idr,
                    source_hdr_required: input
                        .configuration
                        .gpu_processing
                        .as_ref()
                        .map(|selection| selection.source_hdr_required),
                    output_hdr_required: input.configuration.gpu_processing.as_ref().map(|_| {
                        input.configuration.video_stream_format.dynamic_range
                            == amaterasu_module_sdk::DynamicRange::Hdr
                    }),
                    timestamp_us: input.timestamp_us,
                    sequence: input.sequence,
                    outcome: Outcome::NotDelivered,
                    fresh: None,
                    presentation: None,
                    frame: None,
                    encoder_submit_us: 0,
                },
                encoder,
                processor,
                video: input.video,
                calls: 0,
                invalid: false,
            },
            selected,
            calls: 0,
            invalid: false,
        };
        let sink = CaptureResourceFrameSink {
            context: (&mut delivery as *mut Delivery<'_>).cast(),
            write: frame,
        };
        let started = Instant::now();
        let status =
            unsafe { (api.read_resource_frame)(api.base.base.base.context, input.capture, sink) };
        if delivery.invalid
            || delivery.calls > 1
            || (status == STATUS_OK && delivery.calls != 1)
            || (delivery.calls == 1 && status != STATUS_OK)
        {
            return Err("capture_resource_protocol");
        }
        if status == MEDIA_UNSUPPORTED {
            if let Some(cached) = cache.get_mut(&input.video) {
                cached.generation = 0;
                cached.reconfigure_started.get_or_insert_with(Instant::now);
            }
            continue;
        }
        if !matches!(status, STATUS_OK | MEDIA_BUSY | MEDIA_PENDING) {
            return Err("capture_failed");
        }
        if status == STATUS_OK
            && let Some(cached) = cache.get_mut(&input.video)
        {
            cached.reconfigure_started = None;
        }
        let common = delivery.common.common;
        let fallback = match common.outcome {
            Outcome::Rejected => return Err("frame_rejected"),
            Outcome::Unsupported(reason) => Some(reason),
            _ => None,
        };
        return Ok(Progress {
            dropped: common.outcome == Outcome::Busy,
            submitted: common.outcome == Outcome::Accepted,
            repeat_attempted: !input.capture_due,
            capture_reused: common.fresh == Some(false),
            dmabuf_fresh: common.fresh,
            dmabuf_fallback: fallback,
            dmabuf_presentation: common.presentation,
            frame: common.frame,
            capture_callback_us: elapsed_micros(started),
            capture_callback_count: 1,
            capture_frame_count: u64::from(delivery.calls == 1),
            encoder_submit_us: common.encoder_submit_us,
            ..Progress::default()
        });
    }
    if cache.contains_key(&input.video) {
        Ok(Progress {
            capture_deferred: true,
            ..Progress::default()
        })
    } else {
        Err("capture_resource_stale")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn reconfiguration_wait_has_a_fixed_deadline() {
        let start = Instant::now();
        assert!(!reconfiguration_expired(None, start + RECONFIGURE_TIMEOUT));
        assert!(!reconfiguration_expired(
            Some(start),
            start + RECONFIGURE_TIMEOUT - std::time::Duration::from_nanos(1)
        ));
        assert!(reconfiguration_expired(
            Some(start),
            start + RECONFIGURE_TIMEOUT
        ));
    }

    struct Counts {
        retains: Cell<u32>,
        releases: Cell<u32>,
        sync: Cell<u32>,
        thread: std::thread::ThreadId,
    }
    unsafe extern "C" fn source_retain(context: *mut c_void) -> Status {
        let counts = unsafe { &*context.cast::<Counts>() };
        assert_eq!(counts.thread, std::thread::current().id());
        counts.retains.set(counts.retains.get() + 1);
        STATUS_OK
    }
    unsafe extern "C" fn source_release(context: *mut c_void, completion: *const FrameDescriptor) {
        let counts = unsafe { &*context.cast::<Counts>() };
        assert_eq!(counts.thread, std::thread::current().id());
        counts.releases.set(counts.releases.get() + 1);
        counts.sync.set(unsafe { (*completion).kind });
    }
    fn none() -> FrameDescriptor {
        FrameDescriptor {
            kind: FRAME_SYNC_NONE,
            version: 0,
            size: 0,
            reserved: 0,
            namespace: [0; 16],
            data: ptr::null(),
        }
    }
    #[test]
    fn adapter_pins_generic_frame_until_downstream_release() {
        let counts = Counts {
            retains: Cell::new(0),
            releases: Cell::new(0),
            sync: Cell::new(99),
            thread: std::thread::current().id(),
        };
        let frame = CaptureResourceFrame {
            offer_generation: 1,
            offer_id: 1,
            timestamp_us: 1,
            capture_pacing_timestamp_us: 1,
            sequence: 1,
            width: 1,
            height: 1,
            freshness: CAPTURE_FRAME_FRESH,
            reserved: 0,
            color: Default::default(),
            clock: Default::default(),
            pacing_clock: Default::default(),
            resource: none(),
            acquire_sync: none(),
            context: (&counts as *const Counts).cast_mut().cast(),
            retain: source_retain,
            release: source_release,
        };
        let context = Box::into_raw(Box::new(Adapter {
            frame,
            references: Cell::new(1),
        }))
        .cast::<c_void>();
        assert_eq!(unsafe { retain(context) }, STATUS_OK);
        unsafe { drop_reference(context) };
        assert_eq!(counts.releases.get(), 0);
        unsafe { release(context, -1) };
        assert_eq!(
            (
                counts.retains.get(),
                counts.releases.get(),
                counts.sync.get()
            ),
            (1, 1, FRAME_SYNC_NONE)
        );
    }
    #[test]
    fn null_frame_marks_delivery_invalid_even_if_provider_ignores_sink_error() {
        unsafe extern "C" fn submit_cpu(
            _: *mut c_void,
            _: u64,
            _: *const CpuFrame,
            _: u32,
        ) -> Status {
            MEDIA_UNSUPPORTED
        }
        unsafe extern "C" fn receive(_: *mut c_void, _: u64, _: PacketSink) -> Status {
            MEDIA_PENDING
        }
        unsafe extern "C" fn drain(_: *mut c_void, _: u64) -> Status {
            STATUS_OK
        }
        unsafe extern "C" fn submit_gpu(
            _: *mut c_void,
            _: u64,
            _: *const DmaBufFrame,
            _: u32,
        ) -> Status {
            MEDIA_UNSUPPORTED
        }
        unsafe extern "C" fn submit_lease(
            _: *mut c_void,
            _: u64,
            _: *const DmaBufFrameLease,
            _: u32,
        ) -> Status {
            MEDIA_UNSUPPORTED
        }
        unsafe extern "C" fn cancel(_: *mut c_void, _: u64) {}
        let api = EncoderApiV1_2 {
            base: NativeInterface {
                header: AbiHeader::of::<EncoderApiV1_2>(),
                context: ptr::null_mut(),
                release: None,
            },
            submit: submit_cpu,
            receive,
            drain,
            submit_dmabuf: submit_gpu,
            submit_dmabuf_leased: submit_lease,
            cancel_dmabuf_leases: cancel,
        };
        let legacy = unsafe { &*(&api as *const EncoderApiV1_2).cast::<EncoderApiV1_1>() };
        let caps = DmaBufResourceCapabilities {
            fourcc: 1,
            color: DMABUF_COLOR_SRGB,
            modifier: 0,
            min_width: 1,
            max_width: 1,
            min_height: 1,
            max_height: 1,
            device_id: [0; 16],
        };
        let mut delivery = Delivery {
            common: LeasedSubmission {
                common: Submission {
                    api: legacy,
                    encoder: 1,
                    width: 1,
                    height: 1,
                    force_idr: false,
                    source_hdr_required: None,
                    output_hdr_required: None,
                    timestamp_us: 1,
                    sequence: 1,
                    outcome: Outcome::NotDelivered,
                    fresh: None,
                    presentation: None,
                    frame: None,
                    encoder_submit_us: 0,
                },
                encoder: &api,
                processor: None,
                video: 1,
                calls: 0,
                invalid: false,
            },
            selected: CachedSelection {
                provider: CaptureProvider::Vkms,
                stream: 1,
                generation: 1,
                selection: Selection { id: 1, caps },
                reconfigure_started: None,
            },
            calls: 0,
            invalid: false,
        };
        assert_eq!(
            unsafe { frame((&mut delivery as *mut Delivery<'_>).cast(), ptr::null()) },
            STATUS_INVALID_ARGUMENT
        );
        assert!(delivery.invalid);
    }
    #[test]
    fn offer_requires_retainable_gpu_and_supported_sync() {
        let caps = DmaBufResourceCapabilities {
            fourcc: 1,
            color: DMABUF_COLOR_SRGB,
            modifier: 0,
            min_width: 64,
            max_width: 64,
            min_height: 48,
            max_height: 48,
            device_id: [0; 16],
        };
        let mut candidate = CaptureResourceOffer {
            id: 1,
            ownership: FRAME_OWNERSHIP_RETAINABLE,
            accessibility: FRAME_ACCESS_GPU,
            reserved: 0,
            resource: FrameDescriptor {
                kind: FRAME_RESOURCE_DMABUF,
                version: 1,
                size: size_of::<DmaBufResourceCapabilities>() as u32,
                reserved: 0,
                namespace: [0; 16],
                data: (&caps as *const DmaBufResourceCapabilities).cast(),
            },
            acquire_sync: none(),
            completion_sync: FrameDescriptor {
                kind: FRAME_SYNC_SYNC_FILE,
                version: 1,
                ..none()
            },
        };
        let mut state = Offers {
            ids: vec![],
            selection: None,
            width: 64,
            height: 48,
            invalid: false,
        };
        candidate.ownership = FRAME_OWNERSHIP_BORROWED;
        assert_eq!(
            unsafe { offer((&mut state as *mut Offers).cast(), &candidate) },
            STATUS_OK
        );
        assert!(state.selection.is_none());
        candidate.id = 2;
        candidate.ownership = FRAME_OWNERSHIP_RETAINABLE;
        assert_eq!(
            unsafe { offer((&mut state as *mut Offers).cast(), &candidate) },
            STATUS_OK
        );
        assert_eq!(state.selection.unwrap().id, 2);
        assert_eq!(
            unsafe { offer((&mut state as *mut Offers).cast(), &candidate) },
            STATUS_INVALID_ARGUMENT
        );
        assert!(state.invalid);
        let mut invalid = Offers {
            ids: vec![],
            selection: None,
            width: 64,
            height: 48,
            invalid: false,
        };
        assert_eq!(
            unsafe { offer((&mut invalid as *mut Offers).cast(), ptr::null()) },
            STATUS_INVALID_ARGUMENT
        );
        assert!(invalid.invalid);
    }
}

#[cfg(test)]
mod color_tests {
    use super::*;

    #[test]
    fn rejects_contradictory_color_without_inventing_a_conversion() {
        let valid = FrameColorMetadata {
            version: 1,
            size: size_of::<FrameColorMetadata>() as u32,
            primaries: FRAME_PRIMARIES_BT709,
            transfer: FRAME_TRANSFER_SRGB,
            matrix: FRAME_MATRIX_RGB,
            range: FRAME_RANGE_FULL,
        };
        assert!(color_matches_resource(&valid, DMABUF_COLOR_SRGB));
        assert!(!color_matches_resource(&valid, DMABUF_COLOR_BT2020_PQ));
        for invalid in [
            FrameColorMetadata { size: 0, ..valid },
            FrameColorMetadata {
                version: 2,
                ..valid
            },
            FrameColorMetadata {
                range: FRAME_RANGE_LIMITED,
                ..valid
            },
            FrameColorMetadata {
                matrix: FRAME_MATRIX_BT709,
                ..valid
            },
            FrameColorMetadata {
                transfer: FRAME_TRANSFER_PQ,
                ..valid
            },
        ] {
            assert!(!color_matches_resource(&invalid, DMABUF_COLOR_SRGB));
        }
        assert!(color_matches_resource(
            &FrameColorMetadata {
                primaries: FRAME_COLOR_UNKNOWN,
                transfer: FRAME_COLOR_UNKNOWN,
                ..valid
            },
            DMABUF_COLOR_SRGB
        ));
    }
}
