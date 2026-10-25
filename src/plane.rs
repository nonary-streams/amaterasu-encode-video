//! Native bindings stay on the module tick thread. Only copied packet queues cross threads.
mod dmabuf;
#[cfg(amaterasu_swscale)]
mod preprocess;
mod resize;
use crate::{
    model::{CaptureProvider, Configuration},
    queue::Queues,
};
use amaterasu_media::*;
use amaterasu_module_sdk::{ModuleResult, NativeBinding, RuntimeClient, abi::*};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::c_void;
use std::sync::{Arc, Mutex};
use std::time::Instant;
pub const ENCODE: &str = "amaterasu.encode/1";

/// How captured pixels reach the encoder for one stream.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FramePath {
    /// Borrowed CPU frames, optionally resized or converted by video.
    #[default]
    Cpu,
    /// Borrowed DMA-BUF descriptors forwarded unchanged inside the capture callback.
    DmaBuf,
}

impl FramePath {
    pub fn name(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::DmaBuf => "dmabuf",
        }
    }
}

pub struct Pump<'a> {
    pub video: u64,
    pub provider: CaptureProvider,
    pub frame_path: FramePath,
    pub capture: u64,
    pub encoder: u64,
    pub configuration: &'a Configuration,
    pub capture_due: bool,
    pub constant_rate: bool,
    pub repeat_due: bool,
    pub repeat_frame: Option<&'a OwnedFrame>,
    pub timestamp_us: u64,
    pub sequence: u64,
    pub force_idr: bool,
}
/// What a capture provider reports about its newest completed presentation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresentationQuery {
    /// The provider table predates capture v1.5, or it declined the query;
    /// the stream keeps timer pacing.
    Unavailable,
    /// Nothing has been presented yet, or the provider has no news this tick.
    Pending,
    /// The newest completed presentation; its sequence is never zero.
    Latest(PresentationStatus),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GeometryProgress {
    Ready,
    Busy,
}
#[derive(Clone, Copy, Debug)]
pub struct CaptureArrival {
    /// Provider source time before the video pipeline rewrites frame metadata:
    /// v1.3 supplies WGC acquisition time separately; older tables use the
    /// source frame timestamp.
    pub timestamp_us: u64,
    /// Local monotonic time sampled at callback entry, used to map the source
    /// clock onto the video stream's `Instant` epoch.
    pub observed_at: Instant,
}
#[derive(Default)]
pub struct Progress {
    pub dropped: bool,
    pub submitted: bool,
    /// A bounded asynchronous processor is still busy; retain the due capture
    /// slot so the next available frame does not wait an additional period.
    pub capture_deferred: bool,
    pub repeat_attempted: bool,
    /// The capture provider delivered cached pixels for this scheduled slot.
    /// This is only known when the provider exposes a v1.2-or-newer freshness tail.
    pub capture_reused: bool,
    /// Source timestamp and local observation of a fresh WGC frame. `None`
    /// means freshness is unavailable or no new source frame arrived; encoder
    /// backpressure does not clear observed arrival.
    pub capture_arrival: Option<CaptureArrival>,
    /// Freshness of a delivered DMA-BUF presentation: `Some(true)` for a new
    /// source frame, `Some(false)` for cached reuse.
    pub dmabuf_fresh: Option<bool>,
    /// This stream cannot exchange DMA-BUF frames; the reason selects CPU capture.
    pub dmabuf_fallback: Option<&'static str>,
    /// Provider sequence of the DMA-BUF presentation the capture callback
    /// delivered, whether or not the encoder accepted it.
    pub dmabuf_presentation: Option<u64>,
    pub frame: Option<FrameMetadata>,
    pub owned_frame: Option<OwnedFrame>,
    pub capture_callback_us: u64,
    pub capture_callback_count: u64,
    pub capture_frame_count: u64,
    pub encoder_submit_us: u64,
    pub frame_copy_us: u64,
    pub preprocessing_us: u64,
    pub encoder_poll_us: u64,
    pub transport_admission_us: u64,
    pub transport_admission_count: u64,
}

/// An owned copy of a previously accepted capture frame. The native capture
/// allocation is never retained beyond its synchronous callback.
pub struct OwnedFrame {
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub stride: u32,
    pub bytes: Vec<u8>,
}

/// Non-secret metadata from a frame accepted by the encoder. The frame bytes
/// remain borrowed only during the native callback and are never retained.
#[derive(Clone, Copy)]
pub struct FrameMetadata {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub timestamp_us: u64,
    pub sequence: u64,
}
pub trait Plane {
    fn verify_geometry(
        &mut self,
        provider: CaptureProvider,
        capture: u64,
        expected: Option<CaptureGeometry>,
    ) -> Result<GeometryProgress, &'static str>;
    fn pump(&mut self, input: Pump<'_>) -> Result<Progress, &'static str>;
    fn drain(&mut self, encoder: u64) -> Result<bool, &'static str>;
    fn release_stream(&mut self, _: u64) {}
    fn geometry(
        &mut self,
        _: CaptureProvider,
        _: u64,
    ) -> Result<Option<CaptureGeometry>, &'static str> {
        Ok(None)
    }
    /// Why `provider` cannot deliver DMA-BUF frames directly to the encoder
    /// binding, or `None` when both native tables carry the DMA-BUF tails.
    fn dmabuf_unavailable(&self, _: CaptureProvider) -> Option<&'static str> {
        Some("dmabuf_unsupported")
    }
    /// The newest completed presentation of a DMA-BUF capture stream. This
    /// never blocks and never delivers a frame.
    fn latest_presentation(
        &mut self,
        _: CaptureProvider,
        _: u64,
    ) -> Result<PresentationQuery, &'static str> {
        Ok(PresentationQuery::Unavailable)
    }
}

pub struct NativePlane {
    captures: BTreeMap<CaptureProvider, NativeBinding>,
    /// Optional providers whose startup binding failed, with the reason.
    unbound: BTreeMap<CaptureProvider, &'static str>,
    encoder: NativeBinding,
    queues: Arc<Mutex<Queues>>,
    #[cfg(amaterasu_swscale)]
    processors: preprocess::Processors,
}

impl NativePlane {
    pub fn bind(
        runtime: &RuntimeClient,
        queues: Arc<Mutex<Queues>>,
        providers: &BTreeSet<CaptureProvider>,
    ) -> ModuleResult<Self> {
        let mut captures = BTreeMap::new();
        let mut unbound = BTreeMap::new();
        // SAFETY: signed dependencies retain configured providers through binding
        // destruction. Calls stay on the module tick thread. Only the v1.0
        // prefixes are required; optional tails are checked before each use.
        unsafe {
            for provider in providers {
                match runtime.bind_native(provider.service(), 1, 0, CAPTURE_API_V1_0_SIZE) {
                    Ok(binding) => {
                        captures.insert(*provider, binding);
                    }
                    Err(_) if provider.binding_is_optional() => {
                        unbound.insert(*provider, "capture_provider_unbound");
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(Self {
                captures,
                unbound,
                encoder: runtime.bind_native(ENCODE, 1, 0, size_of::<EncoderApi>() as u32)?,
                queues,
                #[cfg(amaterasu_swscale)]
                processors: preprocess::Processors::default(),
            })
        }
    }
}

struct Submission<'a> {
    api: &'a EncoderApi,
    encoder: u64,
    configuration: &'a Configuration,
    status: Status,
    called: bool,
    allow_resize: bool,
    track_wgc_arrival: bool,
    capture_reused: bool,
    capture_arrival: Option<CaptureArrival>,
    force_idr: bool,
    timestamp_us: u64,
    sequence: u64,
    frame: Option<FrameMetadata>,
    owned_frame: Option<OwnedFrame>,
    encoder_submit_us: u64,
    frame_copy_us: u64,
}

unsafe extern "C" fn submit(context: *mut c_void, frame: *const CpuFrame) -> Status {
    if context.is_null() || frame.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: stack context and capture frame live through synchronous read_frame.
    let submission = unsafe { &mut *context.cast::<Submission<'_>>() };
    let frame = unsafe { &*frame };
    // SAFETY: the capture provider borrows the frame through this callback.
    unsafe { submit_frame(submission, frame, None, None) }
}

unsafe extern "C" fn submit_with_freshness(
    context: *mut c_void,
    frame: *const CpuFrame,
    freshness: u32,
) -> Status {
    if context.is_null() || frame.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: capture provider retains stack context and frame through callback.
    let submission = unsafe { &mut *context.cast::<Submission<'_>>() };
    let frame = unsafe { &*frame };
    let is_fresh = match freshness {
        CAPTURE_FRAME_FRESH => true,
        CAPTURE_FRAME_CACHED_REUSE => false,
        _ => return STATUS_INVALID_ARGUMENT,
    };
    // SAFETY: the capture provider borrows the frame through this callback.
    unsafe { submit_frame(submission, frame, Some(is_fresh), None) }
}

unsafe extern "C" fn submit_with_capture_timestamp(
    context: *mut c_void,
    capture_frame: *const CaptureFrameV1_3,
    freshness: u32,
) -> Status {
    if context.is_null() || capture_frame.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: the provider retains the callback context and wrapper through this call.
    let submission = unsafe { &mut *context.cast::<Submission<'_>>() };
    let capture_frame = unsafe { &*capture_frame };
    let is_fresh = match freshness {
        CAPTURE_FRAME_FRESH => true,
        CAPTURE_FRAME_CACHED_REUSE => false,
        _ => return STATUS_INVALID_ARGUMENT,
    };
    // SAFETY: the provider borrows the frame bytes through this callback.
    unsafe {
        submit_frame(
            submission,
            &capture_frame.frame,
            Some(is_fresh),
            Some(capture_frame.capture_pacing_timestamp_us),
        )
    }
}

unsafe fn submit_frame(
    submission: &mut Submission<'_>,
    frame: &CpuFrame,
    source_is_fresh: Option<bool>,
    capture_pacing_timestamp_us: Option<u64>,
) -> Status {
    submission.called = true;
    if source_is_fresh == Some(false) {
        submission.capture_reused = true;
    }
    if submission.track_wgc_arrival && source_is_fresh != Some(false) {
        submission.capture_arrival = Some(CaptureArrival {
            timestamp_us: capture_pacing_timestamp_us.unwrap_or(frame.timestamp_us),
            observed_at: Instant::now(),
        });
    }
    if frame.validate().is_err() {
        submission.status = STATUS_INVALID_ARGUMENT;
        return STATUS_OK;
    }
    let size_matches = frame.width == submission.configuration.width
        && frame.height == submission.configuration.height;
    let resize_started = Instant::now();
    let resized = if size_matches {
        None
    } else if submission.allow_resize {
        // SAFETY: the capture provider retains the validated allocation until
        // this synchronous callback returns; the resizer copies every pixel.
        let result = unsafe { frame.bytes() }.and_then(|pixels| {
            resize::fit_bgra(
                frame,
                pixels,
                submission.configuration.width,
                submission.configuration.height,
            )
        });
        match result {
            Ok(frame) => Some(frame),
            Err(status) => {
                submission.status = status;
                return STATUS_OK;
            }
        }
    } else {
        submission.status = STATUS_INVALID_ARGUMENT;
        return STATUS_OK;
    };
    let normalized;
    let frame = if let Some(resized) = &resized {
        submission.frame_copy_us = elapsed_micros(resize_started);
        normalized = CpuFrame {
            width: resized.width,
            height: resized.height,
            format: resized.format,
            stride: resized.stride,
            data: resized.bytes.as_ptr(),
            len: resized.bytes.len(),
            timestamp_us: frame.timestamp_us,
            sequence: frame.sequence,
        };
        &normalized
    } else {
        frame
    };
    submission.status = {
        let mut forwarded = *frame;
        forwarded.timestamp_us = submission.timestamp_us;
        forwarded.sequence = submission.sequence;
        submission.frame = Some(FrameMetadata {
            width: frame.width,
            height: frame.height,
            stride: frame.stride,
            timestamp_us: submission.timestamp_us,
            sequence: submission.sequence,
        });
        // SAFETY: validated borrowed capture frame remains live; encoder copies on acceptance.
        let started = Instant::now();
        let status = unsafe {
            (submission.api.submit)(
                submission.api.base.context,
                submission.encoder,
                &forwarded,
                u32::from(submission.force_idr),
            )
        };
        submission.encoder_submit_us = elapsed_micros(started);
        status
    };
    if submission.status == STATUS_OK {
        if let Some(resized) = resized {
            submission.owned_frame = Some(resized);
            return STATUS_OK;
        }
        // SAFETY: capture bytes remain borrowed until read_frame returns. A failed
        // allocation only disables repeats; the encoder already copied its input.
        if let Ok(bytes) = unsafe { frame.bytes() } {
            let started = Instant::now();
            let mut owned = Vec::new();
            if owned.try_reserve_exact(bytes.len()).is_ok() {
                owned.extend_from_slice(bytes);
                submission.owned_frame = Some(OwnedFrame {
                    width: frame.width,
                    height: frame.height,
                    format: frame.format,
                    stride: frame.stride,
                    bytes: owned,
                });
            }
            submission.frame_copy_us = elapsed_micros(started);
        }
    }
    // Capture ownership ends here even on encoder backpressure: deliberately drop frame.
    STATUS_OK
}

struct Destination<'a> {
    queues: &'a Arc<Mutex<Queues>>,
    video: u64,
    transport_admission_us: u64,
    transport_admission_count: u64,
}

unsafe extern "C" fn packet(context: *mut c_void, packet: *const EncodedPacket) -> Status {
    if packet.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: encoder borrows packet; destination lives through synchronous receive.
    let destination = unsafe { &mut *context.cast::<Destination<'_>>() };
    let started = Instant::now();
    destination.transport_admission_count = destination.transport_admission_count.saturating_add(1);
    let status = if let Ok(mut queues) = destination.queues.try_lock() {
        unsafe { queues.push(destination.video, &*packet) }
    } else {
        MEDIA_BUSY
    };
    destination.transport_admission_us = elapsed_micros(started);
    status
}

impl Plane for NativePlane {
    fn verify_geometry(
        &mut self,
        provider: CaptureProvider,
        capture: u64,
        expected: Option<CaptureGeometry>,
    ) -> Result<GeometryProgress, &'static str> {
        let binding = self
            .captures
            .get(&provider)
            .ok_or("capture_provider_unavailable")?;
        verify_capture_geometry(binding, capture, expected)
    }

    fn geometry(
        &mut self,
        provider: CaptureProvider,
        capture: u64,
    ) -> Result<Option<CaptureGeometry>, &'static str> {
        let binding = self
            .captures
            .get(&provider)
            .ok_or("capture_provider_unavailable")?;
        // SAFETY: the validated binding retains the complete capture table.
        let prefix =
            unsafe { binding.table::<CaptureApiPrefix>() }.map_err(|_| "capture_interface")?;
        if !geometry_tail_available(prefix.base.header)? {
            return Ok(None);
        }
        let api = unsafe { binding.table::<CaptureApi>() }.map_err(|_| "capture_interface")?;
        let mut geometry = std::mem::MaybeUninit::uninit();
        // SAFETY: the provider initializes this output only on STATUS_OK.
        match unsafe { (api.geometry)(api.base.context, capture, geometry.as_mut_ptr()) } {
            STATUS_OK => {
                let geometry = unsafe { geometry.assume_init() };
                geometry
                    .validate()
                    .map_err(|_| "invalid_capture_geometry")?;
                Ok(Some(geometry))
            }
            MEDIA_BUSY | MEDIA_PENDING | MEDIA_UNSUPPORTED => Ok(None),
            _ => Err("capture_geometry_failed"),
        }
    }

    fn dmabuf_unavailable(&self, provider: CaptureProvider) -> Option<&'static str> {
        let Some(binding) = self.captures.get(&provider) else {
            return Some(
                self.unbound
                    .get(&provider)
                    .copied()
                    .unwrap_or("capture_provider_unavailable"),
            );
        };
        // SAFETY: both bindings were validated with their complete v1.0 prefixes.
        let capture = unsafe { binding.table::<CaptureApiPrefix>() };
        let encoder = unsafe { self.encoder.table::<EncoderApi>() };
        match (capture, encoder) {
            (Ok(capture), Ok(encoder)) => {
                dmabuf::path_unavailable(capture.base.header, encoder.base.header)
            }
            (Err(_), _) => Some("capture_interface"),
            (_, Err(_)) => Some("encoder_interface"),
        }
    }

    fn latest_presentation(
        &mut self,
        provider: CaptureProvider,
        capture: u64,
    ) -> Result<PresentationQuery, &'static str> {
        let binding = self
            .captures
            .get(&provider)
            .ok_or("capture_provider_unavailable")?;
        // SAFETY: the binding retains its validated v1.0 prefix. The v1.5 table
        // is read only after its advertised minor and size cover the query.
        let prefix =
            unsafe { binding.table::<CaptureApiPrefix>() }.map_err(|_| "capture_interface")?;
        if !dmabuf::presentation_tail_available(prefix.base.header) {
            return Ok(PresentationQuery::Unavailable);
        }
        let api = unsafe { binding.table::<CaptureApiV1_5>() }.map_err(|_| "capture_interface")?;
        dmabuf::latest_presentation(api, capture)
    }

    fn pump(&mut self, input: Pump<'_>) -> Result<Progress, &'static str> {
        // SAFETY: configured services promise these layouts; bindings remain live.
        let encoder_api =
            unsafe { self.encoder.table::<EncoderApi>() }.map_err(|_| "encoder_interface")?;
        if input.frame_path == FramePath::DmaBuf {
            let progress = self.deliver_dmabuf(&input)?;
            return self.receive_packets(encoder_api, &input, progress);
        }
        #[cfg(amaterasu_swscale)]
        let processed = if input.provider == CaptureProvider::Pipewire
            && input.configuration.color_mode.is_default()
            && input.capture != 0
        {
            let binding = self
                .captures
                .get(&input.provider)
                .ok_or("capture_provider_unavailable")?;
            // SAFETY: this binding retained its validated capture prefix.
            let capture_api =
                unsafe { binding.table::<CaptureApiPrefix>() }.map_err(|_| "capture_interface")?;
            Some(self.processors.pump(capture_api, encoder_api, &input)?)
        } else {
            None
        };
        #[cfg(not(amaterasu_swscale))]
        let processed: Option<Progress> = None;
        let progress = if let Some(progress) = processed {
            progress
        } else if input.capture_due {
            let binding = self
                .captures
                .get(&input.provider)
                .ok_or("capture_provider_unavailable")?;
            // SAFETY: the v1.0 prefix was validated at bind time and read_frame is
            // part of that prefix, so old providers do not expose an optional tail.
            let capture_api =
                unsafe { binding.table::<CaptureApiPrefix>() }.map_err(|_| "capture_interface")?;
            let capture_timestamp_api = if input.provider == CaptureProvider::Wgc
                && capture_timestamp_tail_available(capture_api.base.header)?
            {
                // SAFETY: v1.3 minor and full table size were verified above.
                Some(
                    unsafe { binding.table::<CaptureApiV1_3>() }
                        .map_err(|_| "capture_interface")?,
                )
            } else {
                None
            };
            let freshness_api = if capture_timestamp_api.is_none()
                && input.provider == CaptureProvider::Wgc
                && freshness_tail_available(capture_api.base.header)?
            {
                // SAFETY: both v1.2 minor and full table size were verified above.
                Some(
                    unsafe { binding.table::<CaptureApiV1_2>() }
                        .map_err(|_| "capture_interface")?,
                )
            } else {
                None
            };
            capture(
                capture_api,
                capture_timestamp_api,
                freshness_api,
                encoder_api,
                &input,
            )?
        } else {
            Progress::default()
        };
        let progress = if !progress.submitted && !progress.dropped && input.repeat_due {
            if let Some(frame) = input.repeat_frame {
                repeat(encoder_api, frame, &input)?
            } else {
                progress
            }
        } else {
            progress
        };
        self.receive_packets(encoder_api, &input, progress)
    }

    fn drain(&mut self, encoder: u64) -> Result<bool, &'static str> {
        // SAFETY: exact encoder v1 binding stays live through the synchronous call.
        let api = unsafe { self.encoder.table::<EncoderApi>() }.map_err(|_| "encoder_interface")?;
        match unsafe { (api.drain)(api.base.context, encoder) } {
            STATUS_OK => Ok(true),
            MEDIA_BUSY | MEDIA_PENDING => Ok(false),
            _ => Err("encoder_drain_failed"),
        }
    }

    fn release_stream(&mut self, video: u64) {
        #[cfg(amaterasu_swscale)]
        self.processors.remove(video);
        #[cfg(not(amaterasu_swscale))]
        let _ = video;
    }
}

impl NativePlane {
    /// Reads one DMA-BUF presentation for a due capture, or re-reads the latest
    /// one as a keepalive repeat when only a repeat is due.
    fn deliver_dmabuf(&self, input: &Pump<'_>) -> Result<Progress, &'static str> {
        if !input.capture_due && !input.repeat_due {
            return Ok(Progress::default());
        }
        let binding = self
            .captures
            .get(&input.provider)
            .ok_or("capture_provider_unavailable")?;
        // SAFETY: both bindings retain validated v1.0 prefixes; the tails are
        // read only after their advertised minor and size cover them.
        let capture_prefix =
            unsafe { binding.table::<CaptureApiPrefix>() }.map_err(|_| "capture_interface")?;
        let encoder_prefix =
            unsafe { self.encoder.table::<EncoderApi>() }.map_err(|_| "encoder_interface")?;
        if let Some(reason) =
            dmabuf::path_unavailable(capture_prefix.base.header, encoder_prefix.base.header)
        {
            return Ok(Progress {
                dmabuf_fallback: Some(reason),
                ..Progress::default()
            });
        }
        let capture_api =
            unsafe { binding.table::<CaptureApiV1_4>() }.map_err(|_| "capture_interface")?;
        let encoder_api =
            unsafe { self.encoder.table::<EncoderApiV1_1>() }.map_err(|_| "encoder_interface")?;
        dmabuf::deliver(capture_api, encoder_api, input, !input.capture_due)
    }

    /// Polls at most one encoded packet into the stream's bounded queue.
    fn receive_packets(
        &self,
        encoder_api: &EncoderApi,
        input: &Pump<'_>,
        mut progress: Progress,
    ) -> Result<Progress, &'static str> {
        let (encoder_poll_us, transport_admission_us, destination_count) =
            receive(encoder_api, input.encoder, input.video, &self.queues)?;
        progress.encoder_poll_us = encoder_poll_us;
        progress.transport_admission_us = transport_admission_us;
        progress.transport_admission_count = destination_count;
        Ok(progress)
    }
}

fn capture(
    capture: &CaptureApiPrefix,
    capture_timestamp_api: Option<&CaptureApiV1_3>,
    freshness_api: Option<&CaptureApiV1_2>,
    encoder: &EncoderApi,
    input: &Pump<'_>,
) -> Result<Progress, &'static str> {
    let mut submission = Submission {
        api: encoder,
        encoder: input.encoder,
        configuration: input.configuration,
        status: MEDIA_PENDING,
        called: false,
        allow_resize: input.provider == CaptureProvider::Pipewire,
        track_wgc_arrival: input.provider == CaptureProvider::Wgc
            && (!input.constant_rate || capture_timestamp_api.is_some() || freshness_api.is_some()),
        capture_reused: false,
        capture_arrival: None,
        force_idr: input.force_idr,
        timestamp_us: input.timestamp_us,
        sequence: input.sequence,
        frame: None,
        owned_frame: None,
        encoder_submit_us: 0,
        frame_copy_us: 0,
    };
    let callback_started = Instant::now();
    let result = if let Some(capture_timestamp_api) = capture_timestamp_api {
        let sink = FreshFrameSinkV1_3 {
            context: (&mut submission as *mut Submission<'_>).cast(),
            write: submit_with_capture_timestamp,
        };
        // SAFETY: stack sink context and frame bytes remain live through the
        // synchronous v1.3 callback; encoder submission copies accepted bytes.
        unsafe {
            (capture_timestamp_api.read_frame_with_capture_timestamp)(
                capture_timestamp_api.base.context,
                input.capture,
                sink,
            )
        }
    } else if let Some(freshness_api) = freshness_api {
        let sink = FreshFrameSinkV1_2 {
            context: (&mut submission as *mut Submission<'_>).cast(),
            write: submit_with_freshness,
        };
        // SAFETY: both the stack sink context and frame bytes live through the
        // provider's synchronous callback; encoder submit copies accepted bytes.
        unsafe {
            (freshness_api.read_frame_with_freshness)(
                freshness_api.base.context,
                input.capture,
                sink,
            )
        }
    } else {
        let sink = FrameSink {
            context: (&mut submission as *mut Submission<'_>).cast(),
            write: submit,
        };
        // SAFETY: stack sink borrows one frame and encoder copies accepted input.
        unsafe { (capture.read_frame)(capture.base.context, input.capture, sink) }
    };
    if !matches!(result, STATUS_OK | MEDIA_PENDING | MEDIA_BUSY) {
        return Err("capture_failed");
    }
    if submission.called && !matches!(submission.status, STATUS_OK | MEDIA_BUSY) {
        return Err("frame_rejected");
    }
    Ok(Progress {
        dropped: submission.called && submission.status == MEDIA_BUSY,
        submitted: submission.called && submission.status == STATUS_OK,
        repeat_attempted: false,
        capture_reused: submission.capture_reused,
        capture_arrival: submission.capture_arrival,
        frame: (submission.called && submission.status == STATUS_OK)
            .then_some(submission.frame)
            .flatten(),
        owned_frame: submission.owned_frame,
        capture_callback_us: elapsed_micros(callback_started),
        capture_callback_count: 1,
        capture_frame_count: u64::from(submission.called),
        encoder_submit_us: submission.encoder_submit_us,
        frame_copy_us: submission.frame_copy_us,
        ..Progress::default()
    })
}

fn capture_timestamp_tail_available(header: AbiHeader) -> Result<bool, &'static str> {
    if header.minor < CAPTURE_PACING_TIMESTAMP_INTERFACE_MINOR {
        return Ok(false);
    }
    if header.size < CAPTURE_API_V1_3_SIZE {
        return Err("capture_interface");
    }
    Ok(true)
}

fn freshness_tail_available(header: AbiHeader) -> Result<bool, &'static str> {
    if header.minor < CAPTURE_FRESHNESS_INTERFACE_MINOR {
        return Ok(false);
    }
    if header.size < CAPTURE_API_V1_2_SIZE {
        return Err("capture_interface");
    }
    Ok(true)
}

fn repeat(
    api: &EncoderApi,
    frame: &OwnedFrame,
    input: &Pump<'_>,
) -> Result<Progress, &'static str> {
    let copied_frame = CpuFrame {
        width: frame.width,
        height: frame.height,
        format: frame.format,
        stride: frame.stride,
        data: frame.bytes.as_ptr(),
        len: frame.bytes.len(),
        timestamp_us: input.timestamp_us,
        sequence: input.sequence,
    };
    // SAFETY: bytes are owned by Video for the full synchronous submit callback;
    // the encoder contract copies accepted input before returning.
    let started = Instant::now();
    let status = unsafe {
        (api.submit)(
            api.base.context,
            input.encoder,
            &copied_frame,
            u32::from(input.force_idr),
        )
    };
    if !matches!(status, STATUS_OK | MEDIA_BUSY) {
        return Err("frame_rejected");
    }
    Ok(Progress {
        dropped: status == MEDIA_BUSY,
        submitted: status == STATUS_OK,
        repeat_attempted: true,
        capture_reused: false,
        capture_arrival: None,
        frame: None,
        owned_frame: None,
        encoder_submit_us: elapsed_micros(started),
        ..Progress::default()
    })
}

fn verify_capture_geometry(
    binding: &NativeBinding,
    capture: u64,
    expected: Option<CaptureGeometry>,
) -> Result<GeometryProgress, &'static str> {
    // SAFETY: capture bindings were requested with the complete v1.0 prefix.
    let prefix = unsafe { binding.table::<CaptureApiPrefix>() }.map_err(|_| "capture_interface")?;
    let header = prefix.base.header;
    if !geometry_tail_available(header)? {
        return if expected.is_none() {
            Ok(GeometryProgress::Ready)
        } else {
            Err("capture_geometry_unavailable")
        };
    }
    // SAFETY: the header advertises the geometry minor and full table size.
    // NativeBinding additionally validates the advertised size and alignment.
    let api = unsafe { binding.table::<CaptureApi>() }.map_err(|_| "capture_interface")?;
    verify_geometry_callback(api, capture, expected)
}

fn geometry_tail_available(header: AbiHeader) -> Result<bool, &'static str> {
    if header.minor < CAPTURE_INTERFACE_MINOR {
        return Ok(false);
    }
    if header.size < size_of::<CaptureApi>() as u32 {
        return Err("capture_interface");
    }
    Ok(true)
}

fn verify_geometry_callback(
    api: &CaptureApi,
    capture: u64,
    expected: Option<CaptureGeometry>,
) -> Result<GeometryProgress, &'static str> {
    let mut actual = std::mem::MaybeUninit::<CaptureGeometry>::uninit();
    // SAFETY: the provider initializes `actual` exactly when returning STATUS_OK.
    let status = unsafe { (api.geometry)(api.base.context, capture, actual.as_mut_ptr()) };
    match status {
        STATUS_OK => {
            // SAFETY: STATUS_OK is the ABI promise that the output was initialized.
            let actual = unsafe { actual.assume_init() };
            if actual.validate().is_err() || expected != Some(actual) {
                Err("capture_geometry_changed")
            } else {
                Ok(GeometryProgress::Ready)
            }
        }
        MEDIA_BUSY => Ok(GeometryProgress::Busy),
        MEDIA_UNSUPPORTED if expected.is_none() => Ok(GeometryProgress::Ready),
        MEDIA_UNSUPPORTED => Err("capture_geometry_unavailable"),
        MEDIA_CLOSED => Err("capture_closed"),
        _ => Err("capture_geometry_failed"),
    }
}

fn receive(
    api: &EncoderApi,
    encoder: u64,
    video: u64,
    queues: &Arc<Mutex<Queues>>,
) -> Result<(u64, u64, u64), &'static str> {
    let mut destination = Destination {
        queues,
        video,
        transport_admission_us: 0,
        transport_admission_count: 0,
    };
    let sink = PacketSink {
        context: (&mut destination as *mut Destination<'_>).cast(),
        write: packet,
    };
    // SAFETY: stack sink copies at most one bounded borrowed packet.
    let started = Instant::now();
    match unsafe { (api.receive)(api.base.context, encoder, sink) } {
        STATUS_OK | MEDIA_PENDING | MEDIA_BUSY => Ok((
            elapsed_micros(started),
            destination.transport_admission_us,
            destination.transport_admission_count,
        )),
        _ => Err("encoder_failed"),
    }
}

fn elapsed_micros(started: Instant) -> u64 {
    started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AmdAv1LatencyMode;
    use amaterasu_module_sdk::abi::AbiHeader;
    unsafe extern "C" fn busy(_: *mut c_void, _: u64, _: *const CpuFrame, _: u32) -> Status {
        MEDIA_BUSY
    }
    unsafe extern "C" fn receive(_: *mut c_void, _: u64, _: PacketSink) -> Status {
        MEDIA_PENDING
    }
    unsafe extern "C" fn drain(_: *mut c_void, _: u64) -> Status {
        STATUS_OK
    }
    #[test]
    fn capture_callback_drops_busy_frame_and_rejects_size_mismatch() {
        let api = EncoderApi {
            base: NativeInterface {
                header: AbiHeader::of::<EncoderApi>(),
                context: std::ptr::null_mut(),
                release: None,
            },
            submit: busy,
            receive,
            drain,
        };
        let configuration = Configuration {
            video_stream_format: amaterasu_module_sdk::VideoStreamFormat::H264_SDR,
            width: 16,
            height: 16,
            fps: 60,
            bitrate: 100_000,
            slices_per_frame: 1,
            qp: 28,
            min_threads: 2,
            codec: VideoCodec::H264,
            color_mode: amaterasu_media::ColorMode::default(),
            max_reference_frames: 0,
            nvenc_settings: amaterasu_module_sdk::NvencSettings::default(),
            nvenc_spatial_aq: false,
            amd_av1_latency_mode: AmdAv1LatencyMode::Auto,
            sw_preset: None,
            sw_tune: None,
        };
        let mut submission = Submission {
            api: &api,
            encoder: 1,
            configuration: &configuration,
            status: MEDIA_PENDING,
            called: false,
            allow_resize: false,
            track_wgc_arrival: true,
            capture_reused: false,
            capture_arrival: None,
            force_idr: false,
            timestamp_us: 7,
            sequence: 3,
            frame: None,
            owned_frame: None,
            encoder_submit_us: 0,
            frame_copy_us: 0,
        };
        let bytes = [0; 16 * 16 * 4];
        let mut frame = CpuFrame {
            width: 16,
            height: 16,
            format: PIXEL_BGRA8,
            stride: 64,
            data: bytes.as_ptr(),
            len: bytes.len(),
            timestamp_us: 1,
            sequence: 1,
        };
        let context = (&mut submission as *mut Submission<'_>).cast();
        assert_eq!(unsafe { submit(context, &frame) }, STATUS_OK);
        assert_eq!(submission.status, MEDIA_BUSY);
        assert_eq!(
            submission.capture_arrival.unwrap().timestamp_us,
            frame.timestamp_us
        );
        assert!(submission.owned_frame.is_none());
        frame.width = 8;
        assert_eq!(unsafe { submit(context, &frame) }, STATUS_OK);
        assert_eq!(submission.status, STATUS_INVALID_ARGUMENT);

        submission.track_wgc_arrival = false;
        submission.capture_arrival = None;
        frame.width = 16;
        assert_eq!(unsafe { submit(context, &frame) }, STATUS_OK);
        assert!(submission.capture_arrival.is_none());
    }

    #[test]
    fn v1_3_source_timestamp_does_not_change_encoder_metadata() {
        let mut observed = SubmittedFrames::default();
        let api = EncoderApi {
            base: NativeInterface {
                header: AbiHeader::of::<EncoderApi>(),
                context: (&mut observed as *mut SubmittedFrames).cast(),
                release: None,
            },
            submit: record_frame,
            receive,
            drain,
        };
        let configuration = Configuration {
            video_stream_format: amaterasu_module_sdk::VideoStreamFormat::H264_SDR,
            width: 16,
            height: 16,
            fps: 60,
            bitrate: 100_000,
            slices_per_frame: 1,
            qp: 28,
            min_threads: 2,
            codec: VideoCodec::H264,
            color_mode: amaterasu_media::ColorMode::default(),
            max_reference_frames: 0,
            nvenc_settings: amaterasu_module_sdk::NvencSettings::default(),
            nvenc_spatial_aq: false,
            amd_av1_latency_mode: AmdAv1LatencyMode::Auto,
            sw_preset: None,
            sw_tune: None,
        };
        let bytes = [0x5a; 16 * 16 * 4];
        let captured = CaptureFrameV1_3 {
            frame: CpuFrame {
                width: 16,
                height: 16,
                format: PIXEL_BGRA8,
                stride: 64,
                data: bytes.as_ptr(),
                len: bytes.len(),
                timestamp_us: 700,
                sequence: 900,
            },
            capture_pacing_timestamp_us: 123_456,
        };
        let mut submission = Submission {
            api: &api,
            encoder: 2,
            configuration: &configuration,
            status: MEDIA_PENDING,
            called: false,
            allow_resize: false,
            track_wgc_arrival: true,
            capture_reused: false,
            capture_arrival: None,
            force_idr: false,
            timestamp_us: 12,
            sequence: 3,
            frame: None,
            owned_frame: None,
            encoder_submit_us: 0,
            frame_copy_us: 0,
        };
        let context = (&mut submission as *mut Submission<'_>).cast();

        assert_eq!(
            unsafe { submit_with_capture_timestamp(context, &captured, CAPTURE_FRAME_FRESH) },
            STATUS_OK
        );
        assert_eq!(submission.capture_arrival.unwrap().timestamp_us, 123_456);
        assert_eq!(submission.frame.unwrap().timestamp_us, 12);
        assert_eq!(submission.frame.unwrap().sequence, 3);
        assert_eq!(observed.0.len(), 1);
        assert_eq!(observed.0[0], (3, 12, bytes.to_vec()));

        submission.capture_arrival = None;
        submission.capture_reused = false;
        assert_eq!(
            unsafe {
                submit_with_capture_timestamp(context, &captured, CAPTURE_FRAME_CACHED_REUSE)
            },
            STATUS_OK
        );
        assert!(submission.capture_arrival.is_none());
        assert!(submission.capture_reused);
        assert_eq!(observed.0[1], (3, 12, bytes.to_vec()));
    }

    #[derive(Default)]
    struct SubmittedFrames(Vec<(u64, u64, Vec<u8>)>);

    unsafe extern "C" fn record_frame(
        context: *mut c_void,
        _: u64,
        frame: *const CpuFrame,
        _: u32,
    ) -> Status {
        if frame.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: this test callback is invoked synchronously with a live frame;
        // the frame's allocation remains valid for the duration of the call.
        let frame = unsafe { &*frame };
        let Ok(bytes) = (unsafe { frame.bytes() }) else {
            return STATUS_INVALID_ARGUMENT;
        };
        // SAFETY: test context points to the live SubmittedFrames value.
        unsafe {
            (*context.cast::<SubmittedFrames>()).0.push((
                frame.sequence,
                frame.timestamp_us,
                bytes.to_vec(),
            ));
        }
        STATUS_OK
    }

    #[test]
    fn repeats_use_owned_accepted_pixels_and_advance_video_metadata() {
        let mut observed = SubmittedFrames::default();
        let api = EncoderApi {
            base: NativeInterface {
                header: AbiHeader::of::<EncoderApi>(),
                context: (&mut observed as *mut SubmittedFrames).cast(),
                release: None,
            },
            submit: record_frame,
            receive,
            drain,
        };
        let configuration = Configuration {
            video_stream_format: amaterasu_module_sdk::VideoStreamFormat::H264_SDR,
            width: 16,
            height: 16,
            fps: 30,
            bitrate: 100_000,
            slices_per_frame: 1,
            qp: 28,
            min_threads: 2,
            codec: VideoCodec::H264,
            color_mode: amaterasu_media::ColorMode::default(),
            max_reference_frames: 0,
            nvenc_settings: amaterasu_module_sdk::NvencSettings::default(),
            nvenc_spatial_aq: false,
            amd_av1_latency_mode: AmdAv1LatencyMode::Auto,
            sw_preset: None,
            sw_tune: None,
        };
        let mut source_bytes = [0x6b; 16 * 16 * 4];
        let captured = CpuFrame {
            width: 16,
            height: 16,
            format: PIXEL_BGRA8,
            stride: 64,
            data: source_bytes.as_ptr(),
            len: source_bytes.len(),
            timestamp_us: 1,
            sequence: 1,
        };
        let mut submission = Submission {
            api: &api,
            encoder: 2,
            configuration: &configuration,
            status: MEDIA_PENDING,
            called: false,
            allow_resize: false,
            track_wgc_arrival: true,
            capture_reused: false,
            capture_arrival: None,
            force_idr: false,
            timestamp_us: 12,
            sequence: 3,
            frame: None,
            owned_frame: None,
            encoder_submit_us: 0,
            frame_copy_us: 0,
        };
        let context = (&mut submission as *mut Submission<'_>).cast();
        assert_eq!(unsafe { submit(context, &captured) }, STATUS_OK);
        assert_eq!(submission.status, STATUS_OK);
        assert_eq!(
            submission.capture_arrival.unwrap().timestamp_us,
            captured.timestamp_us
        );
        assert_eq!(submission.frame.unwrap().timestamp_us, 12);
        assert_eq!(submission.frame.unwrap().sequence, 3);
        let owned_frame = submission.owned_frame.as_ref().unwrap();
        assert_eq!(owned_frame.bytes, vec![0x6b; source_bytes.len()]);

        source_bytes.fill(0x19);
        let input = Pump {
            video: 1,
            provider: CaptureProvider::Dxgi,
            frame_path: FramePath::Cpu,
            capture: 1,
            encoder: 2,
            configuration: &configuration,
            capture_due: false,
            constant_rate: false,
            repeat_due: true,
            repeat_frame: Some(owned_frame),
            timestamp_us: 21,
            sequence: 4,
            force_idr: false,
        };
        let repeated = repeat(&api, owned_frame, &input).unwrap();
        assert!(repeated.submitted);
        assert!(repeated.repeat_attempted);
        assert_eq!(observed.0.len(), 2);
        assert_eq!(observed.0[0].0, 3);
        assert_eq!(observed.0[0].1, 12);
        assert_eq!(observed.0[0].2, vec![0x6b; source_bytes.len()]);
        assert_eq!(observed.0[1].0, 4);
        assert_eq!(observed.0[1].1, 21);
        assert_eq!(observed.0[1].2, vec![0x6b; source_bytes.len()]);
    }
    #[test]
    fn portal_resize_submits_and_retains_the_complete_fitted_frame() {
        let mut observed = SubmittedFrames::default();
        let api = EncoderApi {
            base: NativeInterface {
                header: AbiHeader::of::<EncoderApi>(),
                context: (&mut observed as *mut SubmittedFrames).cast(),
                release: None,
            },
            submit: record_frame,
            receive,
            drain,
        };
        let configuration: Configuration = serde_json::from_value(serde_json::json!({
            "width":16,"height":16,"fps":30,"bitrate":100000
        }))
        .unwrap();
        let mut pixels = vec![0x6b; 32 * 32 * 4];
        let captured = CpuFrame {
            width: 32,
            height: 32,
            format: PIXEL_BGRA8,
            stride: 128,
            data: pixels.as_ptr(),
            len: pixels.len(),
            timestamp_us: 1,
            sequence: 1,
        };
        let mut submission = Submission {
            api: &api,
            encoder: 2,
            configuration: &configuration,
            status: MEDIA_PENDING,
            called: false,
            allow_resize: true,
            track_wgc_arrival: false,
            capture_reused: false,
            capture_arrival: None,
            force_idr: false,
            timestamp_us: 12,
            sequence: 3,
            frame: None,
            owned_frame: None,
            encoder_submit_us: 0,
            frame_copy_us: 0,
        };
        // SAFETY: this test retains both the borrowed source and submission
        // context throughout the synchronous capture callback.
        assert_eq!(
            unsafe { submit((&mut submission as *mut Submission<'_>).cast(), &captured) },
            STATUS_OK
        );
        assert_eq!(submission.status, STATUS_OK);
        let metadata = submission.frame.unwrap();
        assert_eq!(
            (metadata.width, metadata.height, metadata.stride),
            (16, 16, 64)
        );
        assert_eq!((metadata.timestamp_us, metadata.sequence), (12, 3));
        pixels.fill(0);
        let retained = submission.owned_frame.unwrap();
        assert_eq!(retained.bytes.len(), 16 * 16 * 4);
        assert!(
            retained
                .bytes
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| *pixel == [0x6b, 0x6b, 0x6b, 255])
        );
        assert_eq!(observed.0[0].2, retained.bytes);
    }

    #[test]
    fn native_submission_passes_force_idr_as_exact_c_flag() {
        unsafe extern "C" fn flag(
            context: *mut c_void,
            _: u64,
            _: *const CpuFrame,
            force: u32,
        ) -> Status {
            // SAFETY: test encoder context points to a live writable flag.
            unsafe {
                *context.cast::<u32>() = force;
            }
            STATUS_OK
        }
        let mut observed = 0u32;
        let api = EncoderApi {
            base: NativeInterface {
                header: AbiHeader::of::<EncoderApi>(),
                context: (&mut observed as *mut u32).cast(),
                release: None,
            },
            submit: flag,
            receive,
            drain,
        };
        let configuration = Configuration {
            video_stream_format: amaterasu_module_sdk::VideoStreamFormat::H264_SDR,
            width: 16,
            height: 16,
            fps: 60,
            bitrate: 100000,
            slices_per_frame: 1,
            qp: 28,
            min_threads: 2,
            codec: VideoCodec::H264,
            color_mode: amaterasu_media::ColorMode::default(),
            max_reference_frames: 0,
            nvenc_settings: amaterasu_module_sdk::NvencSettings::default(),
            nvenc_spatial_aq: false,
            amd_av1_latency_mode: AmdAv1LatencyMode::Auto,
            sw_preset: None,
            sw_tune: None,
        };
        let bytes = [0; 1024];
        let frame = CpuFrame {
            width: 16,
            height: 16,
            format: PIXEL_BGRA8,
            stride: 64,
            data: bytes.as_ptr(),
            len: bytes.len(),
            timestamp_us: 1,
            sequence: 1,
        };
        let mut submission = Submission {
            api: &api,
            encoder: 1,
            configuration: &configuration,
            status: MEDIA_PENDING,
            called: false,
            allow_resize: false,
            track_wgc_arrival: true,
            capture_reused: false,
            capture_arrival: None,
            force_idr: true,
            timestamp_us: 7,
            sequence: 3,
            frame: None,
            owned_frame: None,
            encoder_submit_us: 0,
            frame_copy_us: 0,
        };
        assert_eq!(
            unsafe { submit((&mut submission as *mut Submission<'_>).cast(), &frame) },
            STATUS_OK
        );
        assert_eq!(observed, 1);
        assert_eq!(submission.status, STATUS_OK);
    }

    #[test]
    fn cached_reuse_is_not_recorded_as_a_fresh_capture_arrival() {
        let api = EncoderApi {
            base: NativeInterface {
                header: AbiHeader::of::<EncoderApi>(),
                context: std::ptr::null_mut(),
                release: None,
            },
            submit: busy,
            receive,
            drain,
        };
        let configuration = Configuration {
            video_stream_format: amaterasu_module_sdk::VideoStreamFormat::H264_SDR,
            width: 16,
            height: 16,
            fps: 60,
            bitrate: 100_000,
            slices_per_frame: 1,
            qp: 28,
            min_threads: 2,
            codec: VideoCodec::H264,
            color_mode: amaterasu_media::ColorMode::default(),
            max_reference_frames: 0,
            nvenc_settings: amaterasu_module_sdk::NvencSettings::default(),
            nvenc_spatial_aq: false,
            amd_av1_latency_mode: AmdAv1LatencyMode::Auto,
            sw_preset: None,
            sw_tune: None,
        };
        let bytes = [0; 16 * 16 * 4];
        let frame = CpuFrame {
            width: 16,
            height: 16,
            format: PIXEL_BGRA8,
            stride: 64,
            data: bytes.as_ptr(),
            len: bytes.len(),
            timestamp_us: 1,
            sequence: 1,
        };
        let mut fresh_submission = Submission {
            api: &api,
            encoder: 1,
            configuration: &configuration,
            status: MEDIA_PENDING,
            called: false,
            allow_resize: false,
            track_wgc_arrival: true,
            capture_reused: false,
            capture_arrival: None,
            force_idr: false,
            timestamp_us: 2,
            sequence: 2,
            frame: None,
            owned_frame: None,
            encoder_submit_us: 0,
            frame_copy_us: 0,
        };
        let fresh_context = (&mut fresh_submission as *mut Submission<'_>).cast();
        assert_eq!(
            unsafe { submit_with_freshness(fresh_context, &frame, CAPTURE_FRAME_FRESH) },
            STATUS_OK
        );
        assert!(fresh_submission.capture_arrival.is_some());
        assert!(!fresh_submission.capture_reused);

        let mut cached_submission = Submission {
            api: &api,
            encoder: 1,
            configuration: &configuration,
            status: MEDIA_PENDING,
            called: false,
            allow_resize: false,
            track_wgc_arrival: true,
            capture_reused: false,
            capture_arrival: None,
            force_idr: false,
            timestamp_us: 3,
            sequence: 3,
            frame: None,
            owned_frame: None,
            encoder_submit_us: 0,
            frame_copy_us: 0,
        };
        let cached_context = (&mut cached_submission as *mut Submission<'_>).cast();
        assert_eq!(
            unsafe { submit_with_freshness(cached_context, &frame, CAPTURE_FRAME_CACHED_REUSE) },
            STATUS_OK
        );
        assert!(cached_submission.capture_arrival.is_none());
        assert!(cached_submission.capture_reused);
        assert_eq!(cached_submission.status, MEDIA_BUSY);
    }

    unsafe extern "C" fn no_frame(_: *mut c_void, _: u64, _: FrameSink) -> Status {
        MEDIA_PENDING
    }

    #[test]
    fn old_short_capture_table_does_not_expose_geometry_tail() {
        let prefix = CaptureApiPrefix {
            base: NativeInterface {
                header: AbiHeader {
                    size: CAPTURE_API_V1_0_SIZE,
                    major: 1,
                    minor: 0,
                },
                context: std::ptr::null_mut(),
                release: None,
            },
            read_frame: no_frame,
        };

        assert_eq!(geometry_tail_available(prefix.base.header), Ok(false));
        let expected = CaptureGeometry::new(
            DesktopRect::new(0, 0, 1920, 1080).unwrap(),
            DesktopRect::new(0, 0, 1920, 1080).unwrap(),
        )
        .unwrap();
        assert_eq!(expected.validate(), Ok(()));

        let truncated_current = AbiHeader {
            size: CAPTURE_API_V1_0_SIZE,
            major: 1,
            minor: CAPTURE_INTERFACE_MINOR,
        };
        assert_eq!(
            geometry_tail_available(truncated_current),
            Err("capture_interface")
        );
    }

    #[test]
    fn freshness_tail_requires_both_minor_and_full_table_size() {
        assert_eq!(
            freshness_tail_available(AbiHeader {
                size: CAPTURE_API_V1_1_SIZE,
                major: 1,
                minor: CAPTURE_INTERFACE_MINOR,
            }),
            Ok(false)
        );
        assert_eq!(
            freshness_tail_available(AbiHeader {
                size: CAPTURE_API_V1_2_SIZE,
                major: 1,
                minor: CAPTURE_FRESHNESS_INTERFACE_MINOR,
            }),
            Ok(true)
        );
        assert_eq!(
            freshness_tail_available(AbiHeader {
                size: CAPTURE_API_V1_1_SIZE,
                major: 1,
                minor: CAPTURE_FRESHNESS_INTERFACE_MINOR,
            }),
            Err("capture_interface")
        );
    }

    #[test]
    fn capture_timestamp_tail_requires_both_minor_and_full_table_size() {
        assert_eq!(
            capture_timestamp_tail_available(AbiHeader {
                size: CAPTURE_API_V1_2_SIZE,
                major: 1,
                minor: CAPTURE_FRESHNESS_INTERFACE_MINOR,
            }),
            Ok(false)
        );
        assert_eq!(
            capture_timestamp_tail_available(AbiHeader {
                size: CAPTURE_API_V1_3_SIZE,
                major: 1,
                minor: CAPTURE_PACING_TIMESTAMP_INTERFACE_MINOR,
            }),
            Ok(true)
        );
        assert_eq!(
            capture_timestamp_tail_available(AbiHeader {
                size: CAPTURE_API_V1_2_SIZE,
                major: 1,
                minor: CAPTURE_PACING_TIMESTAMP_INTERFACE_MINOR,
            }),
            Err("capture_interface")
        );
    }

    unsafe extern "C" fn report_geometry(
        context: *mut c_void,
        _: u64,
        output: *mut CaptureGeometry,
    ) -> Status {
        // SAFETY: the test context and output point to live CaptureGeometry storage.
        unsafe { output.write(*context.cast::<CaptureGeometry>()) };
        STATUS_OK
    }

    fn capture_api(geometry: &mut CaptureGeometry) -> CaptureApi {
        CaptureApi {
            base: NativeInterface {
                header: AbiHeader {
                    size: size_of::<CaptureApi>() as u32,
                    major: 1,
                    minor: CAPTURE_INTERFACE_MINOR,
                },
                context: (geometry as *mut CaptureGeometry).cast(),
                release: None,
            },
            read_frame: no_frame,
            geometry: report_geometry,
        }
    }

    #[test]
    fn geometry_tail_requires_valid_exact_snapshot() {
        let expected = CaptureGeometry::new(
            DesktopRect::new(-1920, 0, 1920, 1080).unwrap(),
            DesktopRect::new(-1920, -200, 3840, 1440).unwrap(),
        )
        .unwrap();
        let mut actual = expected;
        let api = capture_api(&mut actual);
        assert_eq!(
            verify_geometry_callback(&api, 1, Some(expected)),
            Ok(GeometryProgress::Ready)
        );

        let mut changed = expected;
        changed.output.left += 1;
        let changed_api = capture_api(&mut changed);
        assert_eq!(
            verify_geometry_callback(&changed_api, 1, Some(expected)),
            Err("capture_geometry_changed")
        );
    }

    unsafe extern "C" fn busy_geometry(_: *mut c_void, _: u64, _: *mut CaptureGeometry) -> Status {
        MEDIA_BUSY
    }

    #[test]
    fn busy_geometry_defers_without_reclassifying_topology() {
        let api = CaptureApi {
            base: NativeInterface {
                header: AbiHeader {
                    size: size_of::<CaptureApi>() as u32,
                    major: 1,
                    minor: CAPTURE_INTERFACE_MINOR,
                },
                context: std::ptr::null_mut(),
                release: None,
            },
            read_frame: no_frame,
            geometry: busy_geometry,
        };
        assert_eq!(
            verify_geometry_callback(&api, 1, None),
            Ok(GeometryProgress::Busy)
        );
    }
}
