//! Native bindings stay on the module tick thread. Only copied packet queues cross threads.
mod capture_events;
mod dmabuf;
#[cfg(test)]
mod freshness_tests;
mod gpu_processor;
mod lease_lifecycle;
#[cfg(test)]
mod preflight_tests;
#[cfg(amaterasu_swscale)]
mod preprocess;
mod processor;
mod provider_output;
mod resize;
mod resource_cpu;
mod resource_generic;
mod resource_processor;
mod source;
use crate::{
    model::{CaptureProvider, Configuration},
    queue::Queues,
};
use amaterasu_media::*;
use amaterasu_module_sdk::{
    ModuleResult, NativeBinding, RuntimeClient, abi::*, media::native_contracts,
};
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
    /// Negotiated producer resource forwarded to an independent consumer.
    Generic,
}

impl FramePath {
    pub fn name(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::DmaBuf => "dmabuf",
            Self::Generic => "generic",
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
    /// Query the selected capture provider without allocating a capture source.
    fn source_preflight(
        &mut self,
        _: CaptureProvider,
        _: &amaterasu_module_sdk::capture::SourcePreflightRequest,
    ) -> Result<amaterasu_module_sdk::capture::SourcePreflightResponse, Status> {
        Err(MEDIA_UNSUPPORTED)
    }
    /// Admit one immutable stream selection before any provider operation.
    fn configure_gpu(
        &mut self,
        _: u64,
        selection: Option<&crate::model::GpuProcessing>,
    ) -> Result<(), &'static str> {
        if selection.is_some() {
            Err("gpu_processor_unavailable")
        } else {
            Ok(())
        }
    }
    /// Replace an active processor's opaque settings between frame operations.
    fn update_gpu(&mut self, _: u64, _: &serde_json::Value) -> Result<(), &'static str> {
        Err("gpu_processor_update_unavailable")
    }
    fn hdr_metadata(
        &mut self,
        _: u64,
        _: CaptureProvider,
        _: u64,
    ) -> Result<Option<HdrMetadata>, &'static str> {
        Ok(None)
    }

    /// Select the trusted service for the stream currently being advanced.
    fn select_encoder(&mut self, _: &str) {}
    fn set_stream_clock(&mut self, _: u64, _: u64, _: [u8; 16]) {}
    fn start_source(
        &mut self,
        _: u64,
        _: u64,
        _: u64,
        _: &Configuration,
        _: u64,
        _: [u8; 16],
    ) -> Result<(), &'static str> {
        Err("encoder_source_unavailable")
    }
    fn pump_source(&mut self, _: u64, _: u64, _: bool) -> Result<bool, &'static str> {
        Err("encoder_source_unavailable")
    }
    fn drain_source(&mut self, _: u64) -> Result<bool, &'static str> {
        Err("encoder_source_unavailable")
    }

    fn verify_geometry(
        &mut self,
        provider: CaptureProvider,
        capture: u64,
        expected: Option<CaptureGeometry>,
    ) -> Result<GeometryProgress, &'static str>;
    fn pump(&mut self, input: Pump<'_>) -> Result<Progress, &'static str>;
    fn drain(&mut self, encoder: u64) -> Result<bool, &'static str>;
    fn release_stream(&mut self, _: u64) {}
    /// Retires ownership after control cleanup has finished for one stream.
    fn finish_stream(&mut self, _: u64) {}
    fn capture_notice(
        &mut self,
        _: u64,
        _: CaptureProvider,
        _: u64,
    ) -> Result<Option<amaterasu_module_sdk::capture::CaptureNotice>, &'static str> {
        Ok(None)
    }
    fn resolve_capture_swap(
        &mut self,
        _: u64,
        _: u64,
        _: amaterasu_module_sdk::capture::SwapDecision,
    ) -> Result<(), &'static str> {
        Err("capture_events_unavailable")
    }
    fn geometry(
        &mut self,
        _: CaptureProvider,
        _: u64,
    ) -> Result<Option<CaptureGeometry>, &'static str> {
        Ok(None)
    }
    /// Why `provider` cannot deliver DMA-BUF frames directly to the encoder
    /// binding, or `None` when both native tables carry the DMA-BUF tails.
    fn dmabuf_unavailable(&self, _: u64, _: CaptureProvider) -> Option<&'static str> {
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
    require_frame_lease: bool,
    resource_processor: Option<resource_processor::Processor>,
    // Destroy downstream bindings before their source bindings. Drop first
    // quiesces every tracked lease while all of these bindings remain alive.
    encoders: BTreeMap<String, NativeBinding>,
    active_encoder: String,
    source_encoders: BTreeMap<String, NativeBinding>,
    resource_consumers: BTreeMap<String, NativeBinding>,
    resource_bridges: BTreeMap<u64, (String, resource_generic::ResourceBridge)>,
    stream_clocks: BTreeMap<u64, (u64, [u8; 16])>,
    sources: BTreeMap<u64, (String, source::Source)>,
    gpu_processor: Option<NativeBinding>,
    gpu_processor_service: Option<String>,
    gpu_processor_opt_in: bool,
    gpu_bypassed: std::collections::BTreeSet<u64>,
    gpu_configured: std::collections::BTreeSet<u64>,
    processor: Option<NativeBinding>,
    captures: BTreeMap<CaptureProvider, NativeBinding>,
    capture_preflights:
        BTreeMap<CaptureProvider, amaterasu_module_sdk::capture::SourcePreflightConnection>,
    /// Optional providers whose startup binding failed, with the reason.
    unbound: BTreeMap<CaptureProvider, &'static str>,
    leased_streams: lease_lifecycle::LeasedStreams,
    capture_events: capture_events::Subscriptions,
    resource_selections: BTreeMap<u64, resource_cpu::CachedSelection>,
    dmabuf_resource_selections: BTreeMap<u64, dmabuf::CachedSelection>,
    queues: Arc<Mutex<Queues>>,
    #[cfg(amaterasu_swscale)]
    processors: preprocess::Processors,
}

impl NativePlane {
    pub fn with_resource_processor(
        mut self,
        runtime: &RuntimeClient,
        service: Option<&str>,
    ) -> ModuleResult<Self> {
        self.resource_processor = service
            .map(|service| resource_processor::Processor::bind(runtime, service))
            .transpose()?;
        Ok(self)
    }
    /// Bind the optional query tail only for signed logical routes. Ordinary
    /// capture selections retain their original bindings and table headers.
    pub fn with_source_preflight_routes(
        mut self,
        runtime: &RuntimeClient,
        routes: &BTreeMap<String, crate::model::SourcePreflightRoute>,
    ) -> Self {
        for route in routes.values() {
            let provider = route.provider;
            if self.captures.contains_key(&provider)
                && !self.capture_preflights.contains_key(&provider)
            {
                // SAFETY: this service is already selected through the exact
                // typed Capture family; bindings retain the runtime/provider.
                if let Ok(binding) = unsafe {
                    amaterasu_module_sdk::capture::SourcePreflightConnection::bind(
                        runtime,
                        provider.service(),
                    )
                } {
                    self.capture_preflights.insert(provider, binding);
                }
            }
        }
        self
    }
    pub fn with_gpu_processor_opt_in(mut self, opt_in: bool) -> Self {
        self.gpu_processor_opt_in = opt_in;
        self
    }
    pub fn with_required_frame_leases(mut self, required: bool) -> Self {
        self.require_frame_lease = required;
        self
    }
    #[allow(clippy::too_many_arguments)] // Independent signed service registries and processors.
    pub fn bind(
        runtime: &RuntimeClient,
        queues: Arc<Mutex<Queues>>,
        providers: &BTreeSet<CaptureProvider>,
        encoder_providers: &BTreeMap<String, String>,
        source_providers: &BTreeMap<String, crate::model::SourceEncoderRegistration>,
        consumer_providers: &BTreeMap<String, String>,
        processor_service: Option<&str>,
        gpu_processor_service: Option<&str>,
    ) -> ModuleResult<Self> {
        let mut captures = BTreeMap::new();
        let mut unbound = BTreeMap::new();
        // SAFETY: signed dependencies retain configured providers through binding
        // destruction. Calls stay on the module tick thread. Only the v1.0
        // prefixes are required; optional tails are checked before each use.
        unsafe {
            for provider in providers {
                match runtime.bind_native_typed(
                    provider.service(),
                    native_contracts::CAPTURE,
                    1,
                    0,
                    CAPTURE_API_V1_0_SIZE,
                ) {
                    Ok(binding) => {
                        captures.insert(*provider, binding);
                    }
                    Err(_) if provider.binding_is_optional() => {
                        unbound.insert(*provider, "capture_provider_unbound");
                    }
                    Err(error) => return Err(error),
                }
            }
            let mut encoders = BTreeMap::new();
            for service in (!providers.is_empty())
                .then_some(ENCODE)
                .into_iter()
                .chain(encoder_providers.values().map(String::as_str))
            {
                encoders.insert(
                    service.to_owned(),
                    runtime.bind_native_typed(
                        service,
                        native_contracts::ENCODER,
                        1,
                        0,
                        size_of::<EncoderApi>() as u32,
                    )?,
                );
            }
            let mut source_encoders = BTreeMap::new();
            for registration in source_providers.values() {
                let binding = runtime.bind_native_typed(
                    &registration.native_service,
                    native_contracts::ENCODER_PROVIDER,
                    ENCODER_PROVIDER_INTERFACE_MAJOR,
                    ENCODER_PROVIDER_INTERFACE_MINOR,
                    ENCODER_PROVIDER_API_V1_0_SIZE,
                )?;
                source_encoders.insert(registration.control_service.clone(), binding);
            }
            let mut resource_consumers = BTreeMap::new();
            for (alias, service) in consumer_providers {
                let control = encoder_providers
                    .get(alias)
                    .ok_or("missing consumer encoder")?;
                let binding = runtime.bind_native_typed(
                    service,
                    native_contracts::FRAME_RESOURCE_CONSUMER,
                    FRAME_RESOURCE_CONSUMER_INTERFACE_MAJOR,
                    FRAME_RESOURCE_CONSUMER_INTERFACE_MINOR,
                    FRAME_RESOURCE_CONSUMER_API_V1_0_SIZE,
                )?;
                resource_consumers.insert(control.clone(), binding);
            }
            Ok(Self {
                require_frame_lease: false,
                resource_processor: None,
                resource_consumers,
                resource_bridges: BTreeMap::new(),
                stream_clocks: BTreeMap::new(),
                source_encoders,
                sources: BTreeMap::new(),
                captures,
                capture_preflights: BTreeMap::new(),
                unbound,
                encoders,
                active_encoder: ENCODE.to_owned(),
                processor: processor_service
                    .map(|service| {
                        runtime.bind_native_typed(
                            service,
                            native_contracts::CPU_PROCESSOR,
                            CPU_PROCESSOR_INTERFACE_MAJOR,
                            CPU_PROCESSOR_INTERFACE_MINOR,
                            CPU_PROCESSOR_API_V1_0_SIZE,
                        )
                    })
                    .transpose()?,
                gpu_processor_service: gpu_processor_service.map(str::to_owned),
                gpu_processor_opt_in: false,
                gpu_bypassed: Default::default(),
                gpu_configured: Default::default(),
                gpu_processor: gpu_processor_service
                    .map(|service| {
                        runtime.bind_native_typed(
                            service,
                            native_contracts::GPU_PROCESSOR,
                            GPU_PROCESSOR_INTERFACE_MAJOR,
                            GPU_PROCESSOR_INTERFACE_MINOR,
                            GPU_PROCESSOR_API_V1_0_SIZE,
                        )
                    })
                    .transpose()?,
                queues,
                leased_streams: lease_lifecycle::LeasedStreams::default(),
                capture_events: capture_events::Subscriptions::new(runtime.work_notifier()),
                resource_selections: BTreeMap::new(),
                dmabuf_resource_selections: BTreeMap::new(),
                #[cfg(amaterasu_swscale)]
                processors: preprocess::Processors::default(),
            })
        }
    }
}

impl Drop for NativePlane {
    fn drop(&mut self) {
        // Processing may borrow source/encoder bindings only until synchronous
        // retirement; cancel every resource session before other bindings drop.
        if let Some(processor) = self.resource_processor.as_mut() {
            processor.finish_all();
        }
        for (_, (service, mut bridge)) in std::mem::take(&mut self.resource_bridges) {
            if let Some(binding) = self.resource_consumers.get(&service)
                && let Ok(api) = unsafe { binding.table::<FrameResourceConsumerApi>() }
            {
                // SAFETY: consumer binding remains live through cancellation.
                unsafe { bridge.cancel(api) };
            }
        }
        for (_, (service, source)) in std::mem::take(&mut self.sources) {
            if let Some(binding) = self.source_encoders.get(&service)
                && let Ok(api) = unsafe { binding.table::<EncoderProviderApi>() }
            {
                // SAFETY: source session is retired before its retained binding.
                unsafe { source.finish(api) };
            }
        }
        self.capture_events.clear();
        // The control shutdown path can only enqueue close requests. Native
        // retirement must complete now, before any provider binding is released.
        self.leased_streams.finish_all();
    }
}

struct Submission<'a> {
    api: &'a EncoderApi,
    processor: Option<&'a CpuProcessorApi>,
    video: u64,
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
    let mut forwarded = *frame;
    forwarded.timestamp_us = submission.timestamp_us;
    forwarded.sequence = submission.sequence;
    submission.status = if let Some(processor) = submission.processor {
        // SAFETY: the capture allocation and processor binding outlive this synchronous call.
        match unsafe {
            processor::dispatch(processor, submission.video, &forwarded, |output| {
                // SAFETY: processor output is borrowed only during this callback.
                submit_to_encoder(submission, output)
            })
        } {
            processor::Outcome::Accepted => STATUS_OK,
            processor::Outcome::Busy => MEDIA_BUSY,
            processor::Outcome::Failed(status) => status,
            processor::Outcome::ProtocolViolation => MEDIA_BACKEND_ERROR,
        }
    } else {
        // SAFETY: the capture allocation remains live for this synchronous submission.
        unsafe { submit_to_encoder(submission, &forwarded) }
    };
    // Capture ownership ends here even on encoder backpressure: deliberately drop frame.
    STATUS_OK
}

/// Submits and retains the exact accepted output, including processor output.
unsafe fn submit_to_encoder(submission: &mut Submission<'_>, frame: &CpuFrame) -> Status {
    if frame.width != submission.configuration.width
        || frame.height != submission.configuration.height
        || frame.validate().is_err()
    {
        return STATUS_INVALID_ARGUMENT;
    }
    submission.frame = Some(FrameMetadata {
        width: frame.width,
        height: frame.height,
        stride: frame.stride,
        timestamp_us: frame.timestamp_us,
        sequence: frame.sequence,
    });
    let started = Instant::now();
    // SAFETY: the frame is borrowed synchronously and the encoder copies accepted bytes.
    let status = unsafe {
        (submission.api.submit)(
            submission.api.base.context,
            submission.encoder,
            frame,
            u32::from(submission.force_idr),
        )
    };
    submission.encoder_submit_us = elapsed_micros(started);
    if status == STATUS_OK {
        // SAFETY: the producer retains the allocation until this callback returns.
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
    status
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
    fn source_preflight(
        &mut self,
        provider: CaptureProvider,
        request: &amaterasu_module_sdk::capture::SourcePreflightRequest,
    ) -> Result<amaterasu_module_sdk::capture::SourcePreflightResponse, Status> {
        self.capture_preflights
            .get(&provider)
            .ok_or(MEDIA_UNSUPPORTED)?
            .query(request)
    }
    fn configure_gpu(
        &mut self,
        stream: u64,
        selection: Option<&crate::model::GpuProcessing>,
    ) -> Result<(), &'static str> {
        if let Some(processor) = self.resource_processor.as_mut() {
            return processor.configure(stream, selection);
        }
        let Some(selection) = selection else {
            if self.gpu_processor_opt_in {
                self.gpu_bypassed.insert(stream);
            }
            return Ok(());
        };
        if self.gpu_processor_service.as_deref() != Some(selection.service.as_str()) {
            return Err("gpu_processor_selection_mismatch");
        }
        let binding = self
            .gpu_processor
            .as_ref()
            .ok_or("gpu_processor_unavailable")?;
        if !selection.enabled {
            self.gpu_bypassed.insert(stream);
            return Ok(());
        }
        let json = serde_json::to_vec(&selection.settings)
            .map_err(|_| "gpu_processor_settings_invalid")?;
        let api = unsafe { gpu_processor::table(binding) }?;
        unsafe { amaterasu_module_sdk::processing::configure_gpu_stream(api, stream, &json) }
            .map_err(|_| "gpu_processor_settings_rejected")?;
        self.gpu_configured.insert(stream);
        Ok(())
    }

    fn update_gpu(
        &mut self,
        stream: u64,
        settings: &serde_json::Value,
    ) -> Result<(), &'static str> {
        if self.resource_processor.is_some() {
            return Err("resource_processor_live_updates_unsupported");
        }
        if !self.gpu_configured.contains(&stream) || self.gpu_bypassed.contains(&stream) {
            return Err("gpu_processor_update_unavailable");
        }
        let binding = self
            .gpu_processor
            .as_ref()
            .ok_or("gpu_processor_unavailable")?;
        let json = serde_json::to_vec(settings).map_err(|_| "gpu_processor_settings_invalid")?;
        let api = unsafe { gpu_processor::table(binding) }?;
        // SAFETY: the binding remains live, and the video tick serializes this
        // call with frame processing and stream retirement.
        unsafe { amaterasu_module_sdk::processing::update_gpu_stream(api, stream, &json) }.map_err(
            |status| {
                if status == amaterasu_module_sdk::abi::STATUS_INTERNAL_ERROR {
                    "gpu_processor_update_internal"
                } else if status == amaterasu_media::MEDIA_UNSUPPORTED {
                    "gpu_processor_update_unsupported"
                } else {
                    "gpu_processor_settings_rejected"
                }
            },
        )
    }

    fn select_encoder(&mut self, service: &str) {
        debug_assert!(
            self.encoders.contains_key(service) || self.source_encoders.contains_key(service)
        );
        self.active_encoder.clear();
        self.active_encoder.push_str(service);
    }

    fn set_stream_clock(&mut self, video: u64, origin: u64, epoch: [u8; 16]) {
        self.stream_clocks.insert(video, (origin, epoch));
    }
    fn start_source(
        &mut self,
        video: u64,
        session: u64,
        source_id: u64,
        configuration: &Configuration,
        clock_origin_us: u64,
        clock_epoch: [u8; 16],
    ) -> Result<(), &'static str> {
        let binding = self
            .source_encoders
            .get(&self.active_encoder)
            .ok_or("encoder_source_unavailable")?;
        // SAFETY: the signed service supplies this independently validated table.
        let api =
            unsafe { binding.table::<EncoderProviderApi>() }.map_err(|_| "encoder_interface")?;
        if !encoder_provider_available(api.base.header) {
            return Err("encoder_interface");
        }
        // SAFETY: control service authorized this session and bindings outlive it.
        let source = unsafe {
            source::Source::open(
                api,
                video,
                session,
                source_id,
                configuration,
                clock_origin_us,
                clock_epoch,
            )
        }?;
        self.sources
            .insert(video, (self.active_encoder.clone(), source));
        Ok(())
    }
    fn pump_source(&mut self, video: u64, _: u64, force_idr: bool) -> Result<bool, &'static str> {
        let (service, source) = self
            .sources
            .get_mut(&video)
            .ok_or("encoder_source_unavailable")?;
        let binding = self
            .source_encoders
            .get(service)
            .ok_or("encoder_source_unavailable")?;
        // SAFETY: this is the same live binding that configured the source.
        let api =
            unsafe { binding.table::<EncoderProviderApi>() }.map_err(|_| "encoder_interface")?;
        let mut queues = self.queues.lock().map_err(|_| "queue_failed")?;
        let result = unsafe { source.pump(api, &mut queues, force_idr) };
        // SAFETY: the same authenticated typed binding and source session are
        // live. Optional telemetry failure cannot stop or consume media output.
        let timing =
            unsafe { amaterasu_module_sdk::media::provider_encode_timing(api, source.session()) }
                .ok()
                .flatten();
        queues.observe_encode_timing(video, service, source.session(), timing);
        result
    }
    fn drain_source(&mut self, video: u64) -> Result<bool, &'static str> {
        let (service, source) = self
            .sources
            .get_mut(&video)
            .ok_or("encoder_source_unavailable")?;
        let binding = self
            .source_encoders
            .get(service)
            .ok_or("encoder_source_unavailable")?;
        // SAFETY: this is the source session's live provider binding.
        let api =
            unsafe { binding.table::<EncoderProviderApi>() }.map_err(|_| "encoder_interface")?;
        unsafe { source.drain(api) }
    }

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

    fn dmabuf_unavailable(&self, video: u64, provider: CaptureProvider) -> Option<&'static str> {
        if self.processor.is_some() {
            return Some("processor_requires_cpu");
        }
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
        let encoder = unsafe {
            self.encoders
                .get(&self.active_encoder)
                .expect("validated encoder binding")
                .table::<EncoderApi>()
        };
        match (capture, encoder) {
            (Ok(capture), Ok(encoder)) => {
                if self.require_frame_lease
                    && let Err(reason) =
                        dmabuf::require_lease_path(capture.base.header, encoder.base.header)
                {
                    return Some(reason);
                }
                if capture_resources_available(capture.base.header) {
                    return if dmabuf::encoder_lease_tail_available(encoder.base.header) {
                        None
                    } else {
                        Some("encoder_dmabuf_lease_interface_unavailable")
                    };
                }
                if let Err(reason) = dmabuf::use_lease_path(
                    capture.base.header,
                    encoder.base.header,
                    self.gpu_processor.is_some() && !self.gpu_bypassed.contains(&video),
                ) {
                    return Some(reason);
                }
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

    fn hdr_metadata(
        &mut self,
        video: u64,
        provider: CaptureProvider,
        stream: u64,
    ) -> Result<Option<HdrMetadata>, &'static str> {
        if let Some(processor) = self.resource_processor.as_ref()
            && processor.is_active(video)
        {
            return Ok(processor.hdr_metadata(video));
        }
        if let Some(binding) = &self.gpu_processor
            && !self.gpu_bypassed.contains(&video)
        {
            return gpu_processor::hdr_metadata(binding, video);
        }
        let Some(binding) = self.captures.get(&provider) else {
            return Ok(None);
        };
        // SAFETY: inspect the prefix before using the optional v1.9 tail.
        let prefix =
            unsafe { binding.table::<NativeInterface>() }.map_err(|_| "capture_interface")?;
        if prefix.header.minor < 9 || prefix.header.size < size_of::<CaptureApiV1_9>() as u32 {
            return Ok(None);
        }
        let api = unsafe { binding.table::<CaptureApiV1_9>() }.map_err(|_| "capture_interface")?;
        let mut metadata = HdrMetadata::default();
        match unsafe { (api.hdr_metadata)(binding.context(), stream, &mut metadata) } {
            STATUS_OK if metadata.enabled <= 1 && metadata.reserved == 0 => Ok(Some(metadata)),
            MEDIA_PENDING | MEDIA_BUSY => Ok(None),
            _ => Err("capture_hdr_metadata_unavailable"),
        }
    }

    fn pump(&mut self, input: Pump<'_>) -> Result<Progress, &'static str> {
        if let Some(binding) = &self.gpu_processor
            && !self.gpu_bypassed.contains(&input.video)
            && let Some(metadata) = gpu_processor::hdr_metadata(binding, input.video)?
        {
            let hdr = input.configuration.video_stream_format.dynamic_range
                == amaterasu_module_sdk::DynamicRange::Hdr;
            if (metadata.enabled == 1) != hdr {
                return Err("processor_output_range_mismatch");
            }
        }
        if self.require_frame_lease && input.frame_path != FramePath::DmaBuf {
            return Err("capture_frame_lease_required");
        }
        if input.configuration.gpu_processing.is_some()
            && input.frame_path != FramePath::DmaBuf
            && self.resource_processor.is_none()
        {
            // Explicit processing policy requires capture-owned color provenance;
            // untagged CPU frames cannot prove the requested source range.
            return Err("capture_range_verification_requires_dmabuf");
        }
        if input.configuration.gpu_processing.is_some() {
            let capture = self
                .captures
                .get(&input.provider)
                .ok_or("capture_provider_unavailable")?;
            // SAFETY: the typed capture binding and stream remain live through
            // this SDK query, which checks the additive table version and size.
            let prefix =
                unsafe { capture.table::<CaptureApiPrefix>() }.map_err(|_| "capture_interface")?;
            match unsafe { amaterasu_module_sdk::capture::color_provenance(prefix, input.capture) }
            {
                Ok(true) => {}
                Err(MEDIA_PENDING | MEDIA_BUSY) => return Ok(Progress::default()),
                _ => return Err("capture_color_provenance_unavailable"),
            }
        }
        if let Some(processor) = self.resource_processor.as_mut()
            && processor.is_active(input.video)
        {
            if input.frame_path != FramePath::Cpu {
                return Err("resource_processor_requires_cpu_resources");
            }
            let capture = self
                .captures
                .get(&input.provider)
                .ok_or("capture_provider_unavailable")?;
            let prefix =
                unsafe { capture.table::<CaptureApiPrefix>() }.map_err(|_| "capture_interface")?;
            if !capture_resources_available(prefix.base.header) {
                return Err("capture_resources_unavailable");
            }
            let capture =
                unsafe { capture.table::<CaptureApiV1_8>() }.map_err(|_| "capture_interface")?;
            let encoder = unsafe {
                self.encoders
                    .get(&self.active_encoder)
                    .ok_or("encoder_interface")?
                    .table::<EncoderApi>()
            }
            .map_err(|_| "encoder_interface")?;
            let progress = processor.pump(capture, encoder, &input)?;
            return self.receive_packets(encoder, &input, progress);
        }
        // SAFETY: configured services promise these layouts; bindings remain live.
        if input.frame_path == FramePath::Generic {
            let progress = self.deliver_generic(&input)?;
            let encoder = unsafe {
                self.encoders
                    .get(&self.active_encoder)
                    .ok_or("encoder_interface")?
                    .table::<EncoderApi>()
            }
            .map_err(|_| "encoder_interface")?;
            return self.receive_packets(encoder, &input, progress);
        }
        if input.frame_path == FramePath::DmaBuf {
            if self.processor.is_some() {
                return Err("processor_requires_cpu");
            }
            let progress = self.deliver_dmabuf(&input)?;
            let encoder_api = unsafe {
                self.encoders
                    .get(&self.active_encoder)
                    .expect("validated encoder binding")
                    .table::<EncoderApi>()
            }
            .map_err(|_| "encoder_interface")?;
            return self.receive_packets(encoder_api, &input, progress);
        }
        // SAFETY: the configured binding retains this validated native prefix.
        let encoder_api = unsafe {
            self.encoders
                .get(&self.active_encoder)
                .expect("validated encoder binding")
                .table::<EncoderApi>()
        }
        .map_err(|_| "encoder_interface")?;
        if self.gpu_processor.is_some() && !self.gpu_bypassed.contains(&input.video) {
            return Err("gpu_processor_requires_dmabuf");
        }
        #[cfg(amaterasu_swscale)]
        let processed = if self.processor.is_none()
            && input.provider == CaptureProvider::Pipewire
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
            let capture_timestamp_api =
                if capture_timestamp_tail_available(capture_api.base.header)? {
                    // SAFETY: v1.3 minor and full table size were verified above.
                    Some(
                        unsafe { binding.table::<CaptureApiV1_3>() }
                            .map_err(|_| "capture_interface")?,
                    )
                } else {
                    None
                };
            // Keep the freshness tail available as a per-stream fallback.
            // Reading the timestamp does not itself enable WGC pacing.
            let freshness_api = if freshness_tail_available(capture_api.base.header)? {
                // SAFETY: both v1.2 minor and full table size were verified above.
                Some(
                    unsafe { binding.table::<CaptureApiV1_2>() }
                        .map_err(|_| "capture_interface")?,
                )
            } else {
                None
            };
            let resource_api = if input.provider.uses_generic_cpu() {
                if !capture_resources_available(capture_api.base.header) {
                    return Err("capture_resources_unavailable");
                }
                // SAFETY: minor and size were both checked above.
                Some(
                    unsafe { binding.table::<CaptureApiV1_8>() }
                        .map_err(|_| "capture_interface")?,
                )
            } else {
                None
            };
            capture(
                capture_api,
                resource_api.map(|api| (api, &mut self.resource_selections)),
                capture_timestamp_api,
                freshness_api,
                encoder_api,
                self.processor
                    .as_ref()
                    .map(|binding| unsafe { processor::table(binding) })
                    .transpose()?,
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
        let api = unsafe {
            self.encoders
                .get(&self.active_encoder)
                .expect("validated encoder binding")
                .table::<EncoderApi>()
        }
        .map_err(|_| "encoder_interface")?;
        match unsafe { (api.drain)(api.base.context, encoder) } {
            STATUS_OK => Ok(true),
            MEDIA_BUSY | MEDIA_PENDING => Ok(false),
            _ => Err("encoder_drain_failed"),
        }
    }

    fn finish_stream(&mut self, video: u64) {
        if let Some(processor) = self.resource_processor.as_mut() {
            processor.finish(video);
        }
        self.release_stream(video);
        self.stream_clocks.remove(&video);
        self.resource_selections.remove(&video);
        self.dmabuf_resource_selections.remove(&video);
        self.capture_events.remove(video);
        let processor_retired = self.leased_streams.finish(video);
        if self.gpu_configured.remove(&video)
            && !processor_retired
            && let Some(binding) = &self.gpu_processor
            && let Ok(api) = unsafe { gpu_processor::table(binding) }
        {
            // SAFETY: the configured binding is live and downstream leases have retired.
            unsafe { (api.cancel_dmabuf_leases)(api.base.context, video) };
        }
        self.gpu_bypassed.remove(&video);
    }

    fn release_stream(&mut self, video: u64) {
        if let Some((service, mut bridge)) = self.resource_bridges.remove(&video)
            && let Some(binding) = self.resource_consumers.get(&service)
            && let Ok(api) = unsafe { binding.table::<FrameResourceConsumerApi>() }
        {
            // SAFETY: the consumer binding remains live until cancellation completes.
            unsafe { bridge.cancel(api) };
        }
        if let Some((service, source)) = self.sources.remove(&video)
            && let Some(binding) = self.source_encoders.get(&service)
            && let Ok(api) = unsafe { binding.table::<EncoderProviderApi>() }
        {
            // SAFETY: the provider binding and control session are still live.
            unsafe { source.finish(api) };
        }
        self.resource_selections.remove(&video);
        self.dmabuf_resource_selections.remove(&video);
        self.capture_events.remove(video);
        #[cfg(amaterasu_swscale)]
        self.processors.remove(video);
        #[cfg(not(amaterasu_swscale))]
        let _ = video;
    }

    fn capture_notice(
        &mut self,
        video: u64,
        provider: CaptureProvider,
        capture: u64,
    ) -> Result<Option<amaterasu_module_sdk::capture::CaptureNotice>, &'static str> {
        let binding = self.captures.get(&provider).ok_or("capture_bind_failed")?;
        self.capture_events.notice(video, capture, binding)
    }

    fn resolve_capture_swap(
        &mut self,
        video: u64,
        id: u64,
        decision: amaterasu_module_sdk::capture::SwapDecision,
    ) -> Result<(), &'static str> {
        self.capture_events.resolve(video, id, decision)
    }
}

impl NativePlane {
    fn deliver_generic(&mut self, input: &Pump<'_>) -> Result<Progress, &'static str> {
        if !input.capture_due {
            return Ok(Progress::default());
        }
        if self.processor.is_some()
            || (self.gpu_processor.is_some() && !self.gpu_bypassed.contains(&input.video))
        {
            return Err("generic_processor_unsupported");
        }
        let capture = self
            .captures
            .get(&input.provider)
            .ok_or("capture_provider_unavailable")?;
        let prefix =
            unsafe { capture.table::<CaptureApiPrefix>() }.map_err(|_| "capture_interface")?;
        if !capture_resources_available(prefix.base.header) {
            return Err("capture_resources_unavailable");
        }
        let capture =
            unsafe { capture.table::<CaptureApiV1_8>() }.map_err(|_| "capture_interface")?;
        let consumer = self
            .resource_consumers
            .get(&self.active_encoder)
            .ok_or("resource_consumer_unavailable")?;
        // SAFETY: these distinct signed bindings are checked for their expected tables.
        let consumer = unsafe { consumer.table::<FrameResourceConsumerApi>() }
            .map_err(|_| "resource_consumer_interface")?;
        let (_, epoch) = self
            .stream_clocks
            .get(&input.video)
            .copied()
            .ok_or("resource_clock_unavailable")?;
        let (_, bridge) = match self.resource_bridges.entry(input.video) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => entry.insert((
                self.active_encoder.clone(),
                resource_generic::ResourceBridge::new(input.capture, input.encoder, 0, epoch)
                    .map_err(|_| "resource_negotiation_failed")?,
            )),
        };
        // SAFETY: both session-owned bindings remain live through cancellation.
        let status = match unsafe {
            bridge.forward(
                capture,
                consumer,
                if input.force_idr {
                    FRAME_RESOURCE_SUBMIT_FORCE_IDR
                } else {
                    0
                },
            )
        } {
            Ok(status) => status,
            Err(resource_generic::ConnectionError::Native(MEDIA_BUSY | MEDIA_PENDING))
                if !bridge.is_retired() =>
            {
                return Ok(Progress {
                    capture_deferred: true,
                    ..Progress::default()
                });
            }
            Err(_) => return Err("resource_negotiation_failed"),
        };
        match status {
            STATUS_OK => Ok(Progress {
                submitted: true,
                capture_frame_count: 1,
                ..Progress::default()
            }),
            MEDIA_BUSY | MEDIA_PENDING => Ok(Progress {
                capture_deferred: true,
                ..Progress::default()
            }),
            _ => Err("resource_submission_failed"),
        }
    }

    /// Reads one DMA-BUF presentation for a due capture, or re-reads the latest
    /// one as a keepalive repeat when only a repeat is due.
    fn deliver_dmabuf(&mut self, input: &Pump<'_>) -> Result<Progress, &'static str> {
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
        let encoder_prefix = unsafe {
            self.encoders
                .get(&self.active_encoder)
                .expect("validated encoder binding")
                .table::<EncoderApi>()
        }
        .map_err(|_| "encoder_interface")?;
        if capture_resources_available(capture_prefix.base.header) {
            if !dmabuf::encoder_lease_tail_available(encoder_prefix.base.header) {
                return Ok(Progress {
                    dmabuf_fallback: Some("encoder_dmabuf_lease_interface_unavailable"),
                    ..Progress::default()
                });
            }
            let capture =
                unsafe { binding.table::<CaptureApiV1_8>() }.map_err(|_| "capture_interface")?;
            let encoder = unsafe {
                self.encoders
                    .get(&self.active_encoder)
                    .expect("validated encoder binding")
                    .table::<EncoderApiV1_2>()
            }
            .map_err(|_| "encoder_interface")?;
            let processor = self
                .gpu_processor
                .as_ref()
                .filter(|_| !self.gpu_bypassed.contains(&input.video))
                .map(|binding| unsafe { gpu_processor::table(binding) })
                .transpose()?;
            // SAFETY: these bindings remain live until mandatory cancellation completes.
            unsafe {
                self.leased_streams
                    .register(input.video, input.encoder, encoder, processor);
            }
            let progress = dmabuf::deliver_resource(
                capture,
                encoder,
                processor,
                input,
                &mut self.dmabuf_resource_selections,
            )?;
            if (processor.is_some() || self.require_frame_lease)
                && let Some(reason) = progress.dmabuf_fallback
            {
                return Err(reason);
            }
            return Ok(progress);
        }
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
        let encoder_api = unsafe {
            self.encoders
                .get(&self.active_encoder)
                .expect("validated encoder binding")
                .table::<EncoderApiV1_1>()
        }
        .map_err(|_| "encoder_interface")?;
        if dmabuf::use_lease_path(
            capture_prefix.base.header,
            encoder_prefix.base.header,
            self.gpu_processor.is_some() && !self.gpu_bypassed.contains(&input.video),
        )? {
            let leased_capture =
                unsafe { binding.table::<CaptureApiV1_6>() }.map_err(|_| "capture_interface")?;
            let processor = self
                .gpu_processor
                .as_ref()
                .filter(|_| !self.gpu_bypassed.contains(&input.video))
                .map(|binding| unsafe { gpu_processor::table(binding) })
                .transpose()?;
            let leased_encoder = unsafe {
                self.encoders
                    .get(&self.active_encoder)
                    .expect("validated encoder binding")
                    .table::<EncoderApiV1_2>()
            }
            .map_err(|_| "encoder_interface")?;
            // Register before calling either native stage: even a rejected
            // submission can have processor-owned GPU work requiring cleanup.
            // SAFETY: all tables are validated and their bindings outlive this
            // plane's mandatory shutdown barrier.
            unsafe {
                self.leased_streams
                    .register(input.video, input.encoder, leased_encoder, processor);
            }
            let progress = dmabuf::deliver_leased(
                leased_capture,
                leased_encoder,
                processor,
                input,
                !input.capture_due,
            )?;
            if let Some(reason) = progress.dmabuf_fallback {
                if (self.gpu_processor.is_some() && !self.gpu_bypassed.contains(&input.video))
                    || self.require_frame_lease
                {
                    return Err(reason);
                }
                if reason == "capture_dmabuf_lease_unsupported" {
                    return dmabuf::deliver(capture_api, encoder_api, input, !input.capture_due);
                }
            }
            Ok(progress)
        } else if self.require_frame_lease {
            Err("capture_frame_lease_required")
        } else if self.gpu_processor.is_some() && !self.gpu_bypassed.contains(&input.video) {
            Err("gpu_processor_requires_frame_lease")
        } else {
            dmabuf::deliver(capture_api, encoder_api, input, !input.capture_due)
        }
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
        // SAFETY: receive uses this same live typed encoder/session. Old tables
        // explicitly provide no timing; no default encoder is consulted.
        let timing =
            unsafe { amaterasu_module_sdk::media::encoder_timing(encoder_api, input.encoder) }
                .ok()
                .flatten();
        if let Ok(mut queues) = self.queues.lock() {
            queues.observe_encode_timing(input.video, &self.active_encoder, input.encoder, timing);
        }
        Ok(progress)
    }
}

fn capture(
    capture: &CaptureApiPrefix,
    resource: Option<(
        &CaptureApiV1_8,
        &mut BTreeMap<u64, resource_cpu::CachedSelection>,
    )>,
    capture_timestamp_api: Option<&CaptureApiV1_3>,
    freshness_api: Option<&CaptureApiV1_2>,
    encoder: &EncoderApi,
    processor: Option<&CpuProcessorApi>,
    input: &Pump<'_>,
) -> Result<Progress, &'static str> {
    let mut submission = Submission {
        api: encoder,
        processor,
        video: input.video,
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
    let uses_resource = resource.is_some();
    let mut result = if let Some((resource_api, resource_selections)) = resource {
        resource_cpu::read(
            resource_api,
            input.capture,
            input.provider,
            resource_selections,
            &mut submission,
        )?
    } else if let Some(capture_timestamp_api) = capture_timestamp_api {
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
    // Optional callbacks may be unavailable for an individual stream. Retry only
    // if no callback ran, preserving at-most-once frame submission.
    if !uses_resource
        && (capture_timestamp_api.is_some() || freshness_api.is_some())
        && result == MEDIA_UNSUPPORTED
        && !submission.called
    {
        if capture_timestamp_api.is_some()
            && let Some(freshness_api) = freshness_api
        {
            let sink = FreshFrameSinkV1_2 {
                context: (&mut submission as *mut Submission<'_>).cast(),
                write: submit_with_freshness,
            };
            // SAFETY: the stack sink and borrowed frame remain live for this call.
            result = unsafe {
                (freshness_api.read_frame_with_freshness)(
                    freshness_api.base.context,
                    input.capture,
                    sink,
                )
            };
        }
        if result == MEDIA_UNSUPPORTED && !submission.called {
            let sink = FrameSink {
                context: (&mut submission as *mut Submission<'_>).cast(),
                write: submit,
            };
            // SAFETY: the stack sink and borrowed frame remain live for this call.
            result = unsafe { (capture.read_frame)(capture.base.context, input.capture, sink) };
        }
    }
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
    #[derive(Default)]
    struct ProcessorFixture {
        mode: u8,
        process_count: usize,
        encoded: Vec<u8>,
    }

    unsafe extern "C" fn fixture_process(
        context: *mut c_void,
        _: u64,
        frame: *const CpuFrame,
        sink: FrameSink,
    ) -> Status {
        // SAFETY: tests retain the fixture and frame for the synchronous callback.
        let mode = {
            let fixture = unsafe { &mut *context.cast::<ProcessorFixture>() };
            fixture.process_count += 1;
            fixture.mode
        };
        if mode == 1 {
            return MEDIA_BUSY;
        }
        if mode == 2 {
            return MEDIA_BACKEND_ERROR;
        }
        let mut bytes = vec![9; 16 * 16 * 4];
        let mut output = unsafe { *frame };
        output.data = bytes.as_mut_ptr();
        let result = unsafe { (sink.write)(sink.context, &output) };
        if mode == 3 {
            MEDIA_BACKEND_ERROR
        } else {
            result
        }
    }
    unsafe extern "C" fn fixture_encode(
        context: *mut c_void,
        _: u64,
        frame: *const CpuFrame,
        _: u32,
    ) -> Status {
        // SAFETY: tests supply a valid fixture and processor-borrowed frame.
        let fixture = unsafe { &mut *context.cast::<ProcessorFixture>() };
        fixture.encoded = unsafe { (&*frame).bytes().unwrap() }.to_vec();
        STATUS_OK
    }

    #[test]
    fn processor_output_is_encoded_once_and_retained_for_repeats() {
        let mut fixture = ProcessorFixture::default();
        let context = (&mut fixture as *mut ProcessorFixture).cast();
        let api = EncoderApi {
            base: NativeInterface {
                header: AbiHeader::of::<EncoderApi>(),
                context,
                release: None,
            },
            submit: fixture_encode,
            receive,
            drain,
        };
        let processor = CpuProcessorApi {
            base: NativeInterface {
                header: AbiHeader::of::<CpuProcessorApi>(),
                context,
                release: None,
            },
            process: fixture_process,
        };
        let configuration: Configuration = serde_json::from_value(
            serde_json::json!({"width":16,"height":16,"fps":60,"bitrate":100000}),
        )
        .unwrap();
        let bytes = [3; 16 * 16 * 4];
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
            processor: Some(&processor),
            video: 42,
            encoder: 1,
            configuration: &configuration,
            status: MEDIA_PENDING,
            called: false,
            allow_resize: false,
            track_wgc_arrival: false,
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
        // SAFETY: fixture, frame and bytes remain live during synchronous submission.
        unsafe { submit_frame(&mut submission, &frame, None, None) };
        assert_eq!(submission.status, STATUS_OK);
        assert_eq!(fixture.encoded, vec![9; bytes.len()]);
        assert_eq!(
            submission.owned_frame.as_ref().unwrap().bytes,
            fixture.encoded
        );
        assert_eq!(fixture.process_count, 1);
        let owned = submission.owned_frame.as_ref().unwrap();
        let input = Pump {
            video: 42,
            provider: CaptureProvider::Dxgi,
            frame_path: FramePath::Cpu,
            capture: 1,
            encoder: 1,
            configuration: &configuration,
            capture_due: false,
            constant_rate: false,
            repeat_due: true,
            repeat_frame: Some(owned),
            timestamp_us: 8,
            sequence: 4,
            force_idr: false,
        };
        assert!(repeat(&api, owned, &input).unwrap().submitted);
        assert_eq!(fixture.process_count, 1);
        assert_eq!(fixture.encoded, vec![9; bytes.len()]);
        for (mode, expected) in [
            (1, MEDIA_BUSY),
            (2, MEDIA_BACKEND_ERROR),
            (3, MEDIA_BACKEND_ERROR),
        ] {
            fixture.mode = mode;
            submission.owned_frame = None;
            unsafe { submit_frame(&mut submission, &frame, None, None) };
            assert_eq!(submission.status, expected);
        }
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
            gpu_processing: None,
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
            processor: None,
            video: 0,
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
            gpu_processing: None,
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
            processor: None,
            video: 0,
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
            gpu_processing: None,
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
            processor: None,
            video: 0,
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
            processor: None,
            video: 0,
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
            gpu_processing: None,
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
            processor: None,
            video: 0,
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
            gpu_processing: None,
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
            processor: None,
            video: 0,
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
            processor: None,
            video: 0,
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
