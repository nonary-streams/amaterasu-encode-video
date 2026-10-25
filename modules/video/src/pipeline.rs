mod encoder;
mod lifecycle;
mod operations;
mod output;
mod owned_capture;
mod pacing;
mod portal;
mod presentation;

use crate::{
    model::{
        AmdAv1LatencyMode, CaptureProvider, Configuration, EncoderSelection, OpenSpec, Operation,
        Settings, TouchSurface, error, is_owned_kwin_output, valid_minimum_fps_target,
    },
    observation::Observations,
    plane::{
        ENCODE, FrameMetadata, FramePath, GeometryProgress, OwnedFrame, Plane, Progress, Pump,
    },
    queue::Queues,
    rpc::{self, Send},
};
use amaterasu_media::{CaptureGeometry, ColorMode, VideoCodec};
use amaterasu_module_sdk::{
    Command, EncoderBackend, EncoderIdentity, FrameMemory, ModuleResult, Response,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Stage {
    X11Enumerate,
    KmsEnumerate,
    SelectEncoder,
    ProbeEncoder,
    Capture,
    FallbackEnumerate,
    Encoder,
    RestartCloseEncoder,
    /// Closing the vkms capture of a running stream before reopening its
    /// output through the CPU provider; the encoder session stays open.
    CpuFallbackCloseCapture,
    Activating,
    Running,
    StopCapture,
    Draining,
    FindEncoder,
    CloseEncoder,
    FindCapture,
    CloseCapture,
    Failed,
}

struct Stream {
    packet_boundaries: bool,
    owner: String,
    provider: CaptureProvider,
    /// Exact signed X11 service used by both control RPC and native pixels.
    x11_capture_service: Option<String>,
    frame_path: FramePath,
    owned_capture: owned_capture::OwnedOutputCapture,
    auto_capture: bool,
    fallback_provider: Option<CaptureProvider>,
    fallback_device_name: Option<String>,
    fallback_error: Option<&'static str>,
    fallback_attempted: bool,
    fallback_offset: usize,
    fallback_pages: usize,
    fallback_last_output: Option<String>,
    fallback_match: Option<String>,
    fallback_enumeration_id: Option<String>,
    constant_rate: bool,
    minimum_fps_target: f64,
    wgc_pacing_smoothing: bool,
    wgc_pacing: pacing::WgcPacing,
    presentation: presentation::PresentationPacing,
    force_idr: bool,
    grace: Option<output::Grace>,
    output: String,
    capture_output: String,
    encoder: Option<EncoderIdentity>,
    fallback_encoder: Option<EncoderIdentity>,
    restart_pending: bool,
    encoder_index: Option<u32>,
    encoder_attempt: u8,
    requested_encoder: EncoderSelection,
    encoder_candidates: VecDeque<EncoderChoice>,
    encoder_probe_attempt: u8,
    encoder_probe_reason: Option<String>,
    configuration: Configuration,
    parent: Option<u64>,
    ending_parent: Option<u64>,
    pending: Option<u64>,
    unsettled: Option<u64>,
    uncertain: Option<Command>,
    settlement_attempts: u8,
    capture: Option<u64>,
    desktop_geometry: Option<CaptureGeometry>,
    touch_port: Option<Value>,
    session: Option<u64>,
    operation: String,
    capture_operation: String,
    capture_attempt: u8,
    no_resources: bool,
    stage: Stage,
    retries: u8,
    error: Option<&'static str>,
    next_frame: Instant,
    opened_at: Instant,
    /// The host clock at `opened_at`; packet timestamps are offsets from it.
    clock_origin_us: u64,
    last_output_at: Option<Instant>,
    last_repeat_attempt_at: Option<Instant>,
    next_repeat_at: Option<Instant>,
    last_output_timestamp_us: Option<u64>,
    last_output_sequence: u64,
    last_pixels: Option<OwnedFrame>,
    dropped: u64,
    submitted: u64,
    timing: StageTimings,
    last_frame: Option<FrameMetadata>,
    unresolved: bool,
}

#[derive(Clone, Copy, Default)]
struct TimingMetric {
    calls: u64,
    total_us: u64,
    max_us: u64,
}

impl TimingMetric {
    fn record(&mut self, elapsed_us: u64) {
        self.calls = self.calls.saturating_add(1);
        self.total_us = self.total_us.saturating_add(elapsed_us);
        self.max_us = self.max_us.max(elapsed_us);
    }
}

#[derive(Clone, Copy, Default)]
struct StageTimings {
    capture_callback: TimingMetric,
    encoder_submit: TimingMetric,
    frame_copy: TimingMetric,
    preprocessing: TimingMetric,
    encoder_poll: TimingMetric,
    transport_admission: TimingMetric,
    captured_frames: u64,
}

impl StageTimings {
    fn record(&mut self, progress: &Progress) {
        if progress.capture_callback_count > 0 {
            self.capture_callback.record(progress.capture_callback_us);
            self.captured_frames = self
                .captured_frames
                .saturating_add(progress.capture_frame_count);
        }
        if progress.submitted || progress.dropped || progress.repeat_attempted {
            self.encoder_submit.record(progress.encoder_submit_us);
        }
        if progress.preprocessing_us > 0 {
            self.preprocessing.record(progress.preprocessing_us);
        }
        if progress.owned_frame.is_some() {
            self.frame_copy.record(progress.frame_copy_us);
        }
        self.encoder_poll.record(progress.encoder_poll_us);
        for _ in 0..progress.transport_admission_count {
            self.transport_admission
                .record(progress.transport_admission_us);
        }
    }
}

impl Stream {
    fn capture_service(&self) -> &str {
        if self.provider == CaptureProvider::X11 {
            self.x11_capture_service
                .as_deref()
                .unwrap_or_else(|| self.provider.service())
        } else {
            self.provider.service()
        }
    }
    fn encoder_operation(&self) -> String {
        format!(
            "{}-encoder-attempt-{}",
            self.operation, self.encoder_attempt
        )
    }

    fn repeat_rate(&self) -> f64 {
        if self.minimum_fps_target == 0.0 {
            f64::from(self.configuration.fps)
        } else {
            self.minimum_fps_target
        }
    }
    /// A repeat resubmits owned CPU pixels, or re-reads the latest DMA-BUF
    /// presentation once the GPU path has delivered one.
    fn has_repeat_source(&self) -> bool {
        self.last_pixels.is_some()
            || (self.frame_path == FramePath::DmaBuf && self.last_output_at.is_some())
    }

    fn record_repeat_deadline(&mut self, now: Instant, repeated: bool, submitted: bool) {
        if !self.provider.uses_periodic_capture() {
            return;
        }
        let period = Duration::from_secs_f64(1.0 / self.repeat_rate());
        if repeated {
            let previous = self.next_repeat_at.unwrap_or(now);
            self.next_repeat_at = Some(periodic_deadline(previous, now, period));
        } else if submitted {
            self.next_repeat_at = now.checked_add(period);
        }
    }
    fn repeat_due(&self, now: Instant) -> bool {
        if self.provider.uses_periodic_capture() {
            return self.has_repeat_source()
                && self.next_repeat_at.is_some_and(|deadline| now >= deadline);
        }
        let target = if self.minimum_fps_target == 0.0 {
            f64::from(self.configuration.fps)
        } else {
            self.minimum_fps_target
        };
        self.last_pixels.is_some()
            && self
                .last_output_at
                .is_some_and(|last| elapsed_at_rate(now, last, target))
            && self
                .last_repeat_attempt_at
                .is_none_or(|last| elapsed_at_rate(now, last, target))
    }

    /// Microseconds of the stream clock at `now`.
    fn stream_clock_us(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.opened_at)
            .as_micros()
            .min(u64::MAX as u128) as u64
    }

    fn next_frame_clock(&self, now: Instant) -> Option<(u64, u64)> {
        self.frame_clock_at(self.stream_clock_us(now))
    }

    /// The next output timestamp and sequence for a frame stamped
    /// `candidate_us`, kept strictly monotonic.
    fn frame_clock_at(&self, candidate_us: u64) -> Option<(u64, u64)> {
        let sequence = self.last_output_sequence.checked_add(1)?;
        let timestamp_us = self
            .last_output_timestamp_us
            .map(|timestamp| timestamp.checked_add(1))
            .unwrap_or(Some(1))?
            .max(candidate_us);
        Some((timestamp_us, sequence))
    }

    /// Asks the capture provider for its newest presentation on the DMA-BUF
    /// path and reports whether this tick is presentation-paced.
    fn observe_presentation(&mut self, plane: &mut dyn Plane) -> Result<bool, &'static str> {
        if self.frame_path != FramePath::DmaBuf {
            self.presentation.deactivate();
            return Ok(false);
        }
        let capture = self.capture.expect("running streams retain their capture");
        let query = plane.latest_presentation(self.provider, capture)?;
        Ok(self.presentation.observe(query))
    }

    /// A presentation-paced read is due for new content: a newer presentation
    /// the frame-rate cap admits, or, until the first frame is accepted, the
    /// timer probe that also covers a source that has not presented yet.
    fn presentation_capture_due(&self, now: Instant) -> CaptureTrigger {
        if self.presentation.fresh_due(now) {
            CaptureTrigger::Presentation
        } else if self.last_output_at.is_none()
            && now >= self.next_frame
            && self.presentation.submit_allowed(now)
        {
            CaptureTrigger::Timer
        } else {
            CaptureTrigger::None
        }
    }

    /// The keepalive rule decides repeats on a presentation-paced stream; a
    /// pending keyframe request also repeats the latest presentation. Both
    /// respect the frame-rate cap.
    fn presentation_repeat_due(&self, now: Instant) -> bool {
        (self.repeat_due(now) || (self.force_idr && self.has_repeat_source()))
            && self.presentation.submit_allowed(now)
    }

    fn response(&self, id: u64) -> Value {
        let state = match self.stage {
            Stage::SelectEncoder
            | Stage::Capture
            | Stage::FallbackEnumerate
            | Stage::Encoder
            | Stage::RestartCloseEncoder
            | Stage::CpuFallbackCloseCapture
            | Stage::X11Enumerate
            | Stage::KmsEnumerate
            | Stage::ProbeEncoder
            | Stage::Activating => "opening",
            Stage::Running => "running",
            Stage::StopCapture | Stage::Draining => "draining",
            Stage::Failed => "cleanup_failed",
            _ => "closing",
        };
        let mut response = json!({
            "ok": true,
            "stream": id,
            "state": state,
            "stage": stage_name(self.stage),
            "encoder": self.encoder,
            "requested_encoder": self.requested_encoder,
            "encoder_probe_attempts": self.encoder_probe_attempt,
            "encoder_probe_reason": self.encoder_probe_reason,
            "packet_boundaries": self.packet_boundaries,
            "supports_packet_boundaries": true,
            "output": self.output,
            "capture_provider":self.provider,
            "frame_path": self.frame_path.name(),
            "constant_rate": self.constant_rate,
            "wgc_pacing_smoothing": self.wgc_pacing_smoothing,
            "minimum_fps_target": self.minimum_fps_target,
            "force_idr_pending":self.force_idr,
            "configuration": self.configuration,
            "dropped_frames": self.dropped,
            "submitted_frames": self.submitted,
            "stage_timing_us": {
                "capture_callback": {"calls":self.timing.capture_callback.calls,"total":self.timing.capture_callback.total_us,"max":self.timing.capture_callback.max_us,"frames":self.timing.captured_frames},
                "encoder_submit": {"calls":self.timing.encoder_submit.calls,"total":self.timing.encoder_submit.total_us,"max":self.timing.encoder_submit.max_us},
                "frame_copy": {"calls":self.timing.frame_copy.calls,"total":self.timing.frame_copy.total_us,"max":self.timing.frame_copy.max_us},
                "preprocessing": {"calls":self.timing.preprocessing.calls,"total":self.timing.preprocessing.total_us,"max":self.timing.preprocessing.max_us},
                "encoder_poll": {"calls":self.timing.encoder_poll.calls,"total":self.timing.encoder_poll.total_us,"max":self.timing.encoder_poll.max_us},
                "transport_admission": {"calls":self.timing.transport_admission.calls,"total":self.timing.transport_admission.total_us,"max":self.timing.transport_admission.max_us}
            },
            "error": self.error,
        });
        self.owned_capture.write_status(&mut response);
        self.presentation.write_status(&mut response);
        if let Some(encoder) = &self.encoder {
            response["encoder"] = json!(encoder);
        }
        if let Some(geometry) = self.desktop_geometry {
            response["desktop_geometry"] = json!(geometry);
        }
        if let Some(touch_port) = &self.touch_port {
            response["touch_port"] = touch_port.clone();
        }
        if let Some(frame) = self.last_frame {
            response["last_capture_frame"] = json!({
                "width": frame.width,
                "height": frame.height,
                "stride": frame.stride,
                "timestamp_us": frame.timestamp_us,
                "sequence": frame.sequence,
            });
        }
        response
    }

    fn cleanup(&mut self, code: &'static str) {
        if let Some(request) = self.pending.take() {
            self.unsettled = Some(request);
        }
        if let Some(parent) = self.parent.take() {
            self.ending_parent = Some(parent);
        }
        self.stage = Stage::FindEncoder;
        self.error = Some(code);
        self.last_pixels = None;
        self.last_output_at = None;
        self.last_repeat_attempt_at = None;
        self.retries = 0;
        self.unresolved = false;
        self.settlement_attempts = 0;
    }
}

pub struct Pipeline {
    enabled: bool,
    providers: BTreeSet<CaptureProvider>,
    x11_capture_service: Option<String>,
    drain_timeout: Duration,
    recent: VecDeque<(String, Value)>,
    epoch: String,
    next_id: u64,
    cursor: usize,
    streams: BTreeMap<u64, Stream>,
    operations: BTreeMap<(String, String), operations::Record>,
    portal: portal::Control,
    observations: Observations,
    pub queues: Arc<Mutex<Queues>>,
}

fn initial_stage(provider: CaptureProvider, encoder: &EncoderSelection) -> Stage {
    if provider == CaptureProvider::X11 {
        Stage::X11Enumerate
    } else if provider == CaptureProvider::Kms {
        Stage::KmsEnumerate
    } else if matches!(encoder, EncoderSelection::Identity(_)) {
        Stage::Capture
    } else {
        Stage::SelectEncoder
    }
}

impl Pipeline {
    pub fn new(settings: Settings, epoch: String, queues: Arc<Mutex<Queues>>) -> Self {
        Self {
            enabled: settings.enabled,
            observations: Observations::new(
                settings.enabled && settings.capture_observation,
                settings.enabled && settings.frame_observation,
            ),
            providers: settings.capture_providers,
            x11_capture_service: settings.x11_capture_service,
            drain_timeout: Duration::from_millis(settings.drain_timeout_ms),
            recent: VecDeque::new(),
            epoch,
            next_id: 0,
            cursor: 0,
            streams: BTreeMap::new(),
            operations: BTreeMap::new(),
            portal: portal::Control::default(),
            queues,
        }
    }

    pub fn handle(
        &mut self,
        caller: &str,
        request: u64,
        operation: Operation,
        send: &mut Send<'_>,
    ) {
        match operation {
            Operation::SelectPortalSource {
                source_type,
                operation_id,
            } => self.portal.select(
                caller,
                request,
                source_type,
                operation_id,
                self.enabled && self.providers.contains(&CaptureProvider::Pipewire),
                send,
            ),
            Operation::PollPortalSelection { operation_id } => self.portal.poll(
                caller,
                request,
                operation_id,
                self.enabled && self.providers.contains(&CaptureProvider::Pipewire),
                send,
            ),
            Operation::CancelPortalSelection { operation_id } => self.portal.cancel(
                caller,
                request,
                operation_id,
                self.enabled && self.providers.contains(&CaptureProvider::Pipewire),
                send,
            ),
            Operation::Open {
                packet_boundaries,
                operation_id,
                capture_provider,
                auto_capture,
                constant_rate,
                wgc_pacing_smoothing,
                minimum_fps_target,
                output,
                encoder,
                configuration,
            } => self.open(
                caller,
                request,
                OpenSpec {
                    packet_boundaries,
                    operation_id,
                    provider: capture_provider,
                    auto_capture,
                    constant_rate,
                    wgc_pacing_smoothing,
                    minimum_fps_target,
                    output,
                    encoder,
                    configuration: *configuration,
                },
                send,
            ),
            Operation::Lookup { operation_id } => self.lookup(caller, request, &operation_id, send),
            Operation::ForceIdr { stream } => {
                self.output_action(caller, request, stream, false, send)
            }
            Operation::Drain { stream } => self.output_action(caller, request, stream, true, send),
            Operation::Status { stream } => {
                let streams: Vec<_> = self
                    .streams
                    .iter()
                    .filter(|(id, value)| {
                        value.owner == caller && stream.is_none_or(|wanted| wanted == **id)
                    })
                    .map(|(id, value)| value.response(*id))
                    .chain(
                        self.recent
                            .iter()
                            .filter(|(owner, value)| {
                                owner == caller
                                    && stream.is_none_or(|wanted| {
                                        value["stream"].as_u64() == Some(wanted)
                                    })
                            })
                            .map(|(_, value)| value.clone()),
                    )
                    .collect();
                rpc::complete(
                    send,
                    request,
                    json!({"ok":true,"enabled":self.enabled,"streams":streams}),
                );
            }
            Operation::Close { stream } => {
                let Some(value) = self
                    .streams
                    .get_mut(&stream)
                    .filter(|value| value.owner == caller)
                else {
                    rpc::complete(send, request, error("not_found"));
                    return;
                };
                if matches!(
                    value.stage,
                    Stage::SelectEncoder
                        | Stage::X11Enumerate
                        | Stage::KmsEnumerate
                        | Stage::ProbeEncoder
                        | Stage::Capture
                        | Stage::FallbackEnumerate
                        | Stage::Encoder
                        | Stage::RestartCloseEncoder
                        | Stage::CpuFallbackCloseCapture
                        | Stage::Activating
                        | Stage::Running
                        | Stage::StopCapture
                        | Stage::Draining
                        | Stage::Failed
                ) {
                    value.cleanup("closed_by_caller");
                }
                if let Ok(mut queues) = self.queues.lock() {
                    queues.close(stream);
                }
                rpc::complete(send, request, value.response(stream));
            }
        }
    }

    fn open(
        &mut self,
        caller: &str,
        request: u64,
        mut specification: OpenSpec,
        send: &mut Send<'_>,
    ) {
        if !self.enabled {
            rpc::complete(send, request, error("disabled"));
            return;
        }
        if let Err(code) = specification.configuration.normalize_codec() {
            rpc::complete(send, request, error(code));
            return;
        }
        if let Err(code) = specification.configuration.nvenc_settings.validate() {
            rpc::complete(send, request, error(code));
            return;
        }
        if specification.configuration.nvenc_settings
            != amaterasu_module_sdk::NvencSettings::default()
        {
            rpc::complete(send, request, error("nvenc_settings_unsupported"));
            return;
        }
        if specification.provider == CaptureProvider::Pipewire {
            // A sentinel is resolved by the trusted provider, which may restore
            // an earlier portal approval without a management selection RPC.
            let named_owned_output = is_owned_kwin_output(&specification.output);
            let output =
                if specification.output == "portal-selected-session-source" || named_owned_output {
                    Some(specification.output.clone())
                } else {
                    self.portal.resolve_output(&specification.output)
                };
            let Some(output) = output else {
                rpc::complete(send, request, error("portal_source_selection_required"));
                return;
            };
            specification.output = output;
        }
        if specification.configuration.amd_av1_latency_mode != AmdAv1LatencyMode::Auto {
            rpc::complete(send, request, error("amd_av1_latency_mode_unsupported"));
            return;
        }
        if !self.admit_operation(caller, request, &specification, send) {
            return;
        }
        let retained_specification = specification.clone();
        let OpenSpec {
            packet_boundaries,
            operation_id,
            provider,
            auto_capture,
            constant_rate,
            wgc_pacing_smoothing,
            minimum_fps_target,
            output,
            encoder,
            configuration,
        } = specification;
        let failure = if !self.providers.contains(&provider) {
            Some("capture_provider_not_allowed")
        } else if constant_rate && provider != CaptureProvider::Wgc {
            Some("constant_rate_requires_wgc")
        } else if !valid_minimum_fps_target(minimum_fps_target)
            || !configuration.validate()
            || encoder.validate().is_err()
            || output.is_empty()
            || output.len() > 512
            || output.chars().any(char::is_control)
        {
            Some("invalid_request")
        } else if self.streams.len() >= 2 {
            Some("stream_limit")
        } else {
            None
        };
        if let Some(code) = failure {
            rpc::complete(send, request, error(code));
            return;
        }
        let Some(id) = self.next_id.checked_add(1) else {
            rpc::complete(send, request, error("stream_limit"));
            return;
        };
        self.next_id = id;
        if let Some(operation_id) = operation_id {
            self.operations.insert(
                (caller.into(), operation_id),
                operations::Record {
                    stream: id,
                    specification: retained_specification,
                },
            );
        }
        self.streams.insert(id, {
            let opened_at = Instant::now();
            let clock_origin_us = amaterasu_media::host_monotonic_micros();
            let wgc_pacing = pacing::WgcPacing::new(wgc_pacing_smoothing, configuration.fps);
            let operation = format!("video-{}-{id}", self.epoch);
            let requested_encoder = encoder.clone();
            let stage = initial_stage(provider, &encoder);
            let (encoder, encoder_index, _) = match encoder {
                EncoderSelection::Identity(identity) => (Some(identity), None, Stage::Capture),
                EncoderSelection::Index(index) => (None, Some(index), Stage::SelectEncoder),
                EncoderSelection::Backend(EncoderBackend::Auto | EncoderBackend::Software) => {
                    (None, None, Stage::SelectEncoder)
                }
            };
            let owned_capture = owned_capture::OwnedOutputCapture::new(
                provider,
                &output,
                self.providers.contains(&CaptureProvider::Vkms),
            );
            let fallback_provider = if auto_capture && !constant_rate {
                alternate_provider(provider).filter(|candidate| self.providers.contains(candidate))
            } else {
                None
            };
            Stream {
                packet_boundaries,
                owner: caller.into(),
                provider,
                x11_capture_service: self.x11_capture_service.clone(),
                frame_path: FramePath::Cpu,
                owned_capture,
                auto_capture,
                fallback_provider,
                fallback_device_name: None,
                fallback_error: None,
                fallback_attempted: false,
                fallback_offset: 0,
                fallback_pages: 0,
                fallback_last_output: None,
                fallback_match: None,
                fallback_enumeration_id: None,
                constant_rate,
                minimum_fps_target,
                wgc_pacing_smoothing,
                wgc_pacing,
                presentation: presentation::PresentationPacing::default(),
                force_idr: false,
                grace: None,
                capture_output: output.clone(),
                output,
                requested_encoder,
                encoder_candidates: VecDeque::new(),
                encoder_probe_attempt: 0,
                encoder_probe_reason: None,
                encoder,
                fallback_encoder: None,
                restart_pending: false,
                encoder_index,
                encoder_attempt: 0,
                configuration,
                parent: Some(request),
                ending_parent: None,
                pending: None,
                unsettled: None,
                uncertain: None,
                settlement_attempts: 0,
                capture: None,
                desktop_geometry: None,
                touch_port: None,
                session: None,
                capture_operation: format!("{operation}-capture-0"),
                operation,
                capture_attempt: 0,
                no_resources: matches!(
                    stage,
                    Stage::SelectEncoder | Stage::X11Enumerate | Stage::KmsEnumerate
                ),
                stage,
                retries: 0,
                error: None,
                next_frame: Instant::now(),
                opened_at,
                clock_origin_us,
                last_output_at: None,
                last_repeat_attempt_at: None,
                next_repeat_at: None,
                last_output_timestamp_us: None,
                last_output_sequence: 0,
                last_pixels: None,
                dropped: 0,
                submitted: 0,
                timing: StageTimings::default(),
                last_frame: None,
                unresolved: false,
            }
        });
    }

    pub fn tick(&mut self, plane: &mut dyn Plane, send: &mut Send<'_>) {
        self.portal.tick(send);
        if self.streams.is_empty() {
            return;
        }
        self.cursor %= self.streams.len();
        let id = *self.streams.keys().nth(self.cursor).unwrap();
        self.cursor += 1;
        let stream = self.streams.get_mut(&id).unwrap();
        if let Some(parent) = stream.parent
            && !matches!(
                send(Command::CheckRequest { request: parent }),
                Ok(Response::Ok)
            )
        {
            stream.cleanup("request_ended");
        }
        if matches!(
            stream.stage,
            Stage::X11Enumerate | Stage::KmsEnumerate | Stage::SelectEncoder | Stage::ProbeEncoder
        ) && stream.opened_at.elapsed() >= Duration::from_secs(5)
        {
            stream.cleanup(
                if matches!(stream.stage, Stage::X11Enumerate | Stage::KmsEnumerate) {
                    "capture_provider_unavailable"
                } else {
                    "encoder_selection_timeout"
                },
            );
        }
        if stream.stage != Stage::Running {
            plane.release_stream(id);
        }
        if matches!(stream.stage, Stage::StopCapture | Stage::Draining) {
            output::tick(stream, id, &self.queues, plane, &mut self.observations);
            if !matches!(stream.stage, Stage::StopCapture | Stage::Draining)
                && let Ok(mut queues) = self.queues.lock()
            {
                queues.close(id);
            }
            if stream.stage == Stage::Draining {
                return;
            }
        }
        if stream.stage == Stage::Running {
            pump(plane, stream, id, &mut self.observations);
            if let Ok(mut queues) = self.queues.lock() {
                queues.set_geometry(id, stream.desktop_geometry);
            }
            if !matches!(
                stream.stage,
                Stage::Running | Stage::RestartCloseEncoder | Stage::CpuFallbackCloseCapture
            ) && let Ok(mut queues) = self.queues.lock()
            {
                queues.close(id);
            }
            return;
        }
        if stream.stage == Stage::Activating {
            match verify_stream_geometry(plane, stream) {
                Ok(GeometryProgress::Ready) => activate_or_resume(stream, id, &self.queues, send),
                Ok(GeometryProgress::Busy) => return,
                Err(code) => stream.cleanup(code),
            }
            return;
        }
        if stream.stage == Stage::Failed {
            return;
        }
        if advance(stream, id, &self.queues, plane, send) {
            if let Some(stream) = self.streams.remove(&id) {
                self.remember_failure(id, &stream);
            }
            if let Ok(mut queues) = self.queues.lock() {
                queues.close(id);
            }
        }
    }

    /// Deliver only after native callbacks have returned. Missing grants and
    /// observer failures cannot change video stream behavior.
    pub fn flush_observations(&mut self, send: &mut dyn FnMut(Command) -> ModuleResult<Response>) {
        self.observations.flush(send);
    }

    pub fn stop_observations(&mut self) {
        self.observations.clear();
    }
}

/// True when `pixels` maps onto the `logical` output rectangle with one scale
/// in KWin's supported range on both axes, allowing for the compositor's
/// rounding of logical sizes to whole units.
fn consistent_output_scale(
    logical_width: u32,
    logical_height: u32,
    pixel_width: u32,
    pixel_height: u32,
) -> bool {
    if logical_width == 0 || logical_height == 0 {
        return false;
    }
    let horizontal = f64::from(pixel_width) / f64::from(logical_width);
    let vertical = f64::from(pixel_height) / f64::from(logical_height);
    let in_range = |scale: f64| (0.99..=5.01).contains(&scale);
    let rounding = 1.0 / f64::from(logical_width.min(logical_height)) + 0.001;
    in_range(horizontal)
        && in_range(vertical)
        && (horizontal - vertical).abs() <= rounding * horizontal.max(vertical)
}

fn verify_stream_geometry(
    plane: &mut dyn Plane,
    stream: &mut Stream,
) -> Result<GeometryProgress, &'static str> {
    let capture = stream.capture.expect("geometry follows capture admission");
    if matches!(
        stream.provider,
        CaptureProvider::Pipewire | CaptureProvider::Vkms
    ) && stream.capture_output.starts_with("kwin:")
    {
        let Some(actual) = plane.geometry(stream.provider, capture)? else {
            return Ok(GeometryProgress::Busy);
        };
        actual.validate().map_err(|_| "invalid_capture_geometry")?;
        // Geometry is in logical desktop units, so a scaled owned output is
        // smaller than its pixels. A new peer can expand the desktop or move an
        // output without invalidating its pixels; a resize that breaks the
        // stream's aspect (one consistent scale on both axes) cannot.
        if !consistent_output_scale(
            actual.output.width,
            actual.output.height,
            stream.configuration.width,
            stream.configuration.height,
        ) {
            return Err("capture_geometry_changed");
        }
        stream.desktop_geometry = Some(actual);
        Ok(GeometryProgress::Ready)
    } else {
        plane.verify_geometry(stream.provider, capture, stream.desktop_geometry)
    }
}

fn elapsed_at_rate(now: Instant, anchor: Instant, frames_per_second: f64) -> bool {
    now.saturating_duration_since(anchor).as_secs_f64() * frames_per_second >= 1.0
}

/// Why a pump tick reads the capture provider for new content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaptureTrigger {
    None,
    /// The 1/fps capture timer elapsed.
    Timer,
    /// A newer completed presentation is ready and the frame-rate cap admits it.
    Presentation,
}

fn pump(plane: &mut dyn Plane, stream: &mut Stream, id: u64, observations: &mut Observations) {
    let now = Instant::now();
    let presentation_paced = match stream.observe_presentation(plane) {
        Ok(paced) => paced,
        Err(code) => {
            stream.cleanup(code);
            return;
        }
    };
    let mut trigger = if presentation_paced {
        stream.presentation_capture_due(now)
    } else if now >= stream.next_frame {
        CaptureTrigger::Timer
    } else {
        CaptureTrigger::None
    };
    let mut capture_slot = (trigger == CaptureTrigger::Timer).then_some(stream.next_frame);
    if trigger != CaptureTrigger::None {
        match verify_stream_geometry(plane, stream) {
            Ok(GeometryProgress::Ready) => {}
            Ok(GeometryProgress::Busy) => {
                if stream.provider == CaptureProvider::Wgc && !stream.constant_rate {
                    stream.wgc_pacing.capture_missed(
                        stream
                            .next_frame
                            .saturating_duration_since(stream.opened_at),
                    );
                }
                stream.next_frame = next_capture_deadline(now, stream.configuration.fps);
                trigger = CaptureTrigger::None;
                capture_slot = None;
            }
            Err(code) => {
                stream.cleanup(code);
                return;
            }
        }
        if trigger == CaptureTrigger::Timer
            && (stream.provider != CaptureProvider::Wgc || stream.constant_rate)
        {
            stream.next_frame = if stream.provider.uses_periodic_capture() {
                periodic_capture_deadline(stream.next_frame, now, stream.configuration.fps)
            } else {
                next_capture_deadline(now, stream.configuration.fps)
            };
        }
    }
    let capture_due = trigger != CaptureTrigger::None;
    let frame_interval = presentation::frame_interval(stream.configuration.fps);
    let repeat_due = if presentation_paced {
        stream.presentation_repeat_due(now)
    } else {
        stream.repeat_due(now)
    };
    let frame_clock = if trigger == CaptureTrigger::Presentation {
        let presented_us = stream
            .presentation
            .fresh_timestamp(stream.stream_clock_us(now), frame_interval);
        stream.frame_clock_at(presented_us)
    } else {
        stream.next_frame_clock(now)
    };
    let Some((timestamp_us, sequence)) = frame_clock else {
        stream.cleanup("frame_clock_exhausted");
        return;
    };
    let capture_marker = capture_due
        .then(|| {
            observations.before_capture(
                stream.configuration.width,
                stream.configuration.height,
                match stream.frame_path {
                    FramePath::Cpu => FrameMemory::CpuBorrowed,
                    FramePath::DmaBuf => FrameMemory::GpuOpaque,
                },
            )
        })
        .flatten();
    match plane.pump(Pump {
        video: id,
        provider: stream.provider,
        frame_path: stream.frame_path,
        capture: stream.capture.unwrap(),
        encoder: stream.session.unwrap(),
        configuration: &stream.configuration,
        capture_due,
        constant_rate: stream.constant_rate,
        repeat_due,
        repeat_frame: stream.last_pixels.as_ref(),
        timestamp_us,
        sequence,
        force_idr: stream.force_idr,
    }) {
        Ok(progress) => {
            observations.after_capture(capture_marker, progress.capture_callback_count > 0);
            observations.record_pump(
                capture_marker.filter(|_| progress.capture_callback_count > 0),
                progress.accepted_frame,
                progress.encoded_packet,
            );
            let completed_at = Instant::now();
            stream.timing.record(&progress);
            stream.owned_capture.record_freshness(&progress);
            if let Some(reason) = progress.dmabuf_fallback {
                if !owned_capture::begin_cpu_capture_fallback(stream, reason) {
                    stream.cleanup(reason);
                }
                return;
            }
            if presentation_paced {
                stream.presentation.record(
                    completed_at,
                    frame_interval,
                    presentation::PresentationDelivery {
                        pushed: trigger == CaptureTrigger::Presentation,
                        repeat: progress.repeat_attempted,
                        delivered: progress.dmabuf_presentation,
                        submitted: progress.submitted,
                    },
                );
            }
            if stream.provider.uses_periodic_capture()
                && progress.capture_deferred
                && let Some(slot) = capture_slot
            {
                stream.next_frame = slot;
            }
            if capture_due
                && stream.provider == CaptureProvider::Wgc
                && (!stream.constant_rate
                    || progress.capture_arrival.is_some()
                    || progress.capture_reused)
            {
                if let Some(arrival) = progress.capture_arrival {
                    let observed_offset = arrival
                        .observed_at
                        .saturating_duration_since(stream.opened_at);
                    let Some(projected_slot) = stream
                        .wgc_pacing
                        .capture_arrived(arrival.timestamp_us, observed_offset)
                    else {
                        stream.cleanup("frame_clock_exhausted");
                        return;
                    };
                    if !schedule_wgc_after_completion(stream, projected_slot, completed_at) {
                        stream.cleanup("frame_clock_exhausted");
                        return;
                    }
                } else if progress.capture_reused {
                    if let Some(projected_slot) = stream.wgc_pacing.cached_reuse_slot()
                        && !schedule_wgc_after_completion(stream, projected_slot, completed_at)
                    {
                        stream.cleanup("frame_clock_exhausted");
                        return;
                    }
                } else {
                    let capture_slot =
                        capture_slot.expect("due WGC capture retains its target slot");
                    stream
                        .wgc_pacing
                        .capture_missed(capture_slot.saturating_duration_since(stream.opened_at));
                    stream.next_frame = next_capture_deadline(now, stream.configuration.fps);
                }
            }
            stream.dropped = stream.dropped.saturating_add(u64::from(progress.dropped));
            stream.submitted = stream
                .submitted
                .saturating_add(u64::from(progress.submitted));
            stream.record_repeat_deadline(
                completed_at,
                progress.repeat_attempted,
                progress.submitted,
            );
            if progress.repeat_attempted {
                stream.last_repeat_attempt_at = Some(completed_at);
            }
            if progress.submitted {
                stream.last_output_at = Some(completed_at);
                stream.last_repeat_attempt_at = None;
                stream.last_output_timestamp_us = Some(timestamp_us);
                stream.last_output_sequence = sequence;
                if let Some(frame) = progress.frame {
                    stream.last_frame = Some(frame);
                    stream.last_pixels = progress.owned_frame;
                }
                stream.force_idr = false;
            }
        }
        Err(code) => {
            observations.discard_capture(capture_marker);
            if !begin_runtime_encoder_recovery(stream, code) {
                stream.cleanup(code);
            }
        }
    }
}

fn schedule_wgc_after_completion(
    stream: &mut Stream,
    projected_slot: Duration,
    completed_at: Instant,
) -> bool {
    let Some((next_slot, missed_slot)) = pacing::next_slot_after_completion(
        projected_slot,
        completed_at.saturating_duration_since(stream.opened_at),
        stream.configuration.fps,
    ) else {
        return false;
    };
    if missed_slot {
        stream.wgc_pacing.capture_missed(projected_slot);
    }
    let Some(next_deadline) = stream.opened_at.checked_add(next_slot) else {
        return false;
    };
    stream.next_frame = next_deadline;
    true
}

fn next_capture_deadline(now: Instant, fps: u32) -> Instant {
    now + Duration::from_micros(1_000_000 / u64::from(fps))
}

fn periodic_capture_deadline(previous: Instant, now: Instant, fps: u32) -> Instant {
    periodic_deadline(previous, now, Duration::from_secs_f64(1.0 / f64::from(fps)))
}

fn periodic_deadline(previous: Instant, now: Instant, period: Duration) -> Instant {
    let elapsed = now.saturating_duration_since(previous);
    let periods = elapsed.as_nanos() / period.as_nanos() + 1;
    u32::try_from(periods)
        .ok()
        .and_then(|count| period.checked_mul(count))
        .and_then(|advance| previous.checked_add(advance))
        .unwrap_or_else(|| now + period)
}

fn advance(
    stream: &mut Stream,
    id: u64,
    queues: &Arc<Mutex<Queues>>,
    plane: &mut dyn Plane,
    send: &mut Send<'_>,
) -> bool {
    if !lifecycle::settle(stream, send) {
        return false;
    }
    if stream.no_resources
        && matches!(
            stream.stage,
            Stage::FindEncoder | Stage::CloseEncoder | Stage::FindCapture | Stage::CloseCapture
        )
    {
        return finish_cleanup(stream);
    }
    if let Some(request) = stream.pending {
        let Some(result) = rpc::result(send, request) else {
            return false;
        };
        if result.is_err()
            || matches!(
                stream.stage,
                Stage::X11Enumerate
                    | Stage::KmsEnumerate
                    | Stage::SelectEncoder
                    | Stage::ProbeEncoder
            )
        {
            // Selection children retain no codec session. Their result must be
            // released before another probe, capture allocation or failure.
            stream.unsettled = Some(request);
        }
        stream.pending = None;
        return apply_result(stream, id, queues, plane, result, send);
    }
    if stream.stage == Stage::Capture {
        owned_capture::select_capture_provider(stream, plane);
        if stream.stage != Stage::Capture {
            return false;
        }
    }
    if stream.stage == Stage::Encoder {
        match verify_stream_geometry(plane, stream) {
            Ok(GeometryProgress::Ready) => {}
            Ok(GeometryProgress::Busy) => return false,
            Err(code) => {
                stream.cleanup(code);
                return false;
            }
        }
    }
    let (service, payload) = request(stream);
    let operation = if stream.stage == Stage::Capture {
        format!(
            "{}-capture-attempt-{}",
            stream.operation, stream.capture_attempt
        )
    } else if stream.stage == Stage::ProbeEncoder {
        format!(
            "{}-probe-encoder-{}",
            stream.operation, stream.encoder_probe_attempt
        )
    } else if stream.stage == Stage::Encoder {
        stream.encoder_operation()
    } else if stream.stage == Stage::FallbackEnumerate {
        format!(
            "{}-fallback-enumerate-{}-{}",
            stream.operation, stream.fallback_offset, stream.retries
        )
    } else {
        format!(
            "{}-{}-{}",
            stream.operation,
            stage_name(stream.stage),
            stream.retries
        )
    };
    let parent = if matches!(
        stream.stage,
        Stage::X11Enumerate
            | Stage::KmsEnumerate
            | Stage::SelectEncoder
            | Stage::ProbeEncoder
            | Stage::Capture
            | Stage::FallbackEnumerate
            | Stage::Encoder
    ) {
        stream.parent
    } else {
        None
    };
    let command = Command::Request {
        service: service.into(),
        payload,
        timeout_ms: 5000,
        operation_id: operation,
        parent,
    };
    match rpc::deliver(send, command.clone()) {
        rpc::Admission::Accepted(request) => {
            if stream.stage == Stage::Capture {
                stream.no_resources = false;
            }
            stream.pending = Some(request);
        }
        rpc::Admission::Rejected(code) => {
            if stream.stage == Stage::Capture
                && owned_capture::begin_vkms_open_fallback(stream, &code)
            {
                return false;
            }
            if stream.stage == Stage::Capture {
                // A running stream reopening capture still owns its encoder.
                stream.no_resources = stream.session.is_none();
                if code == "unavailable" {
                    if !begin_fallback(stream, "unavailable") {
                        failed_step(stream, "unavailable");
                    }
                } else {
                    failed_step(stream, "request_failed");
                }
            } else if stream.stage == Stage::FallbackEnumerate {
                failed_step(stream, stream.fallback_error.unwrap_or("request_failed"));
            } else {
                failed_step(stream, "request_failed");
            }
        }
        rpc::Admission::Uncertain => {
            if stream.stage == Stage::Capture {
                stream.no_resources = false;
            } else if matches!(stream.stage, Stage::X11Enumerate | Stage::KmsEnumerate) {
                stream.no_resources = true;
            }
            stream.uncertain = Some(command);
            failed_step(stream, "request_failed");
        }
    }
    false
}

fn request(stream: &Stream) -> (&str, Value) {
    match stream.stage {
        Stage::X11Enumerate => (stream.capture_service(), json!({"action":"enumerate"})),
        Stage::KmsEnumerate => (stream.capture_service(), json!({"action":"enumerate"})),
        Stage::SelectEncoder => (ENCODE, json!({"action":"enumerate"})),
        Stage::ProbeEncoder => (
            ENCODE,
            json!({
                "action":"probe", "encoder":stream.encoder,
                "configuration":stream.configuration,
            }),
        ),
        Stage::Capture => {
            let mut request = json!({
                "action":"open",
                "output":stream.capture_output,
                "operation_id":stream.capture_operation,
            });
            if stream.constant_rate {
                request["constant_rate"] = json!(true);
            }
            (stream.capture_service(), request)
        }
        Stage::FallbackEnumerate => {
            let mut request = json!({"action":"enumerate"});
            if stream.fallback_offset != 0 {
                request["offset"] = json!(stream.fallback_offset);
            }
            (
                stream
                    .fallback_provider
                    .expect("fallback enumeration has a provider")
                    .service(),
                request,
            )
        }
        Stage::Encoder => (ENCODE, encoder_open_request(stream)),
        Stage::FindEncoder => (
            ENCODE,
            json!({"action":"lookup","operation_id":stream.encoder_operation()}),
        ),
        Stage::CloseEncoder => (
            ENCODE,
            json!({"action":"close","session_id":stream.session}),
        ),
        Stage::RestartCloseEncoder => (
            ENCODE,
            json!({"action":"close","session_id":stream.session}),
        ),
        Stage::FindCapture => (
            stream.capture_service(),
            json!({"action":"lookup","operation_id":stream.capture_operation}),
        ),
        Stage::CloseCapture | Stage::StopCapture | Stage::CpuFallbackCloseCapture => (
            stream.capture_service(),
            json!({"action":"close","stream":stream.capture}),
        ),
        _ => unreachable!("only pending control stages submit"),
    }
}

fn encoder_open_request(stream: &Stream) -> Value {
    let mut request = json!({
        "action": "open",
        "encoder": stream
            .encoder
            .as_ref()
            .expect("encoder stage follows encoder selection"),
        "configuration": stream.configuration,
        "operation_id": stream.encoder_operation(),
    });
    if stream.packet_boundaries {
        request["packet_boundaries"] = json!(true);
    }
    request
}

fn stage_name(stage: Stage) -> &'static str {
    match stage {
        Stage::X11Enumerate => "x11-output-selection",
        Stage::KmsEnumerate => "kms-output-selection",
        Stage::SelectEncoder => "select-encoder",
        Stage::ProbeEncoder => "probe-encoder",
        Stage::Capture => "capture",
        Stage::FallbackEnumerate => "fallback-enumerate",
        Stage::Encoder => "encoder",
        Stage::Activating => "activating",
        Stage::FindEncoder => "find-encoder",
        Stage::CloseEncoder => "close-encoder",
        Stage::RestartCloseEncoder => "restart-close-encoder",
        Stage::CpuFallbackCloseCapture => "cpu-fallback-close-capture",
        Stage::FindCapture => "find-capture",
        Stage::CloseCapture => "close-capture",
        Stage::StopCapture => "drain-close-capture",
        Stage::Running => "running",
        Stage::Draining => "draining",
        Stage::Failed => "cleanup-failed",
    }
}

fn failed_step(stream: &mut Stream, code: &'static str) {
    if matches!(
        stream.stage,
        Stage::X11Enumerate
            | Stage::KmsEnumerate
            | Stage::SelectEncoder
            | Stage::ProbeEncoder
            | Stage::Capture
            | Stage::FallbackEnumerate
            | Stage::Encoder
            | Stage::RestartCloseEncoder
            | Stage::CpuFallbackCloseCapture
            | Stage::Activating
            | Stage::StopCapture
    ) {
        stream.cleanup(code);
    } else {
        stream.retries += 1;
        if stream.retries >= 3 {
            stream.error = Some("cleanup_failed");
            stream.unresolved = true;
            stream.stage = if matches!(stream.stage, Stage::FindEncoder | Stage::CloseEncoder) {
                Stage::FindCapture
            } else {
                Stage::Failed
            };
            stream.retries = 0;
        }
    }
}

fn begin_runtime_encoder_recovery(stream: &mut Stream, code: &'static str) -> bool {
    if !matches!(code, "encoder_failed" | "frame_rejected")
        || stream.stage != Stage::Running
        || stream.encoder_attempt != 0
        || stream.fallback_encoder.is_none()
        || stream.restart_pending
    {
        return false;
    }
    stream.restart_pending = true;
    stream.force_idr = true;
    stream.next_frame = Instant::now();
    stream.retries = 0;
    stream.stage = Stage::RestartCloseEncoder;
    true
}

fn apply_result(
    stream: &mut Stream,
    id: u64,
    queues: &Arc<Mutex<Queues>>,
    plane: &mut dyn Plane,
    result: Result<Value, &'static str>,
    send: &mut Send<'_>,
) -> bool {
    let payload = match result {
        Ok(payload) => payload,
        Err(code) => {
            if matches!(stream.stage, Stage::X11Enumerate | Stage::KmsEnumerate) {
                stream.cleanup("capture_provider_unavailable");
            } else {
                failed_step(stream, code);
            }
            return false;
        }
    };
    if stream.stage == Stage::X11Enumerate {
        match x11_selection_output(&payload, &stream.capture_output, &stream.configuration) {
            Ok(()) => {
                stream.stage = if matches!(stream.requested_encoder, EncoderSelection::Identity(_))
                {
                    Stage::Capture
                } else {
                    Stage::SelectEncoder
                };
                stream.retries = 0;
            }
            Err(code) => {
                stream.no_resources = true;
                stream.cleanup(code);
            }
        }
        return false;
    }
    if stream.stage == Stage::KmsEnumerate {
        match kms_selection_output(&payload, &stream.capture_output, &stream.configuration) {
            Ok(()) => {
                stream.stage = if matches!(stream.requested_encoder, EncoderSelection::Identity(_))
                {
                    Stage::Capture
                } else {
                    Stage::SelectEncoder
                };
                stream.retries = 0;
            }
            Err(code) => {
                stream.no_resources = true;
                stream.cleanup(code);
            }
        }
        return false;
    }
    if stream.stage == Stage::ProbeEncoder {
        let Some(identity) = &stream.encoder else {
            stream.cleanup("invalid_encoder_probe_reply");
            return false;
        };
        match encoder::probe_result(identity, &stream.configuration, &payload) {
            Ok(None) => {
                // A software candidate selected by probing has already used
                // the software fallback; never restart it as its own fallback.
                if stream.encoder == stream.fallback_encoder {
                    stream.fallback_encoder = None;
                }
                stream.stage = Stage::Capture;
                stream.encoder_candidates.clear();
                stream.retries = 0;
            }
            Ok(Some(reason)) => {
                stream.encoder_probe_reason = Some(reason);
                if !begin_encoder_probe(stream) {
                    stream.cleanup("encoder_unavailable");
                }
            }
            Err(code) => stream.cleanup(code),
        }
        return false;
    }
    let success = payload.get("ok").and_then(Value::as_bool) == Some(true);
    let missing = rpc::missing(&payload);
    if stream.stage == Stage::Capture
        && payload.get("ok").and_then(Value::as_bool) == Some(false)
        && owned_capture::begin_vkms_open_fallback(
            stream,
            payload_error_code(&payload).unwrap_or("provider_failed"),
        )
    {
        return false;
    }
    if stream.stage == Stage::Capture
        && payload.get("ok").and_then(Value::as_bool) == Some(false)
        && payload_error_code(&payload) == Some("unsupported_platform")
    {
        stream.no_resources = stream.session.is_none();
        if !begin_fallback(stream, "unsupported_platform") {
            failed_step(stream, "unsupported_platform");
        }
        return false;
    }
    if !success
        && !(missing
            && matches!(
                stream.stage,
                Stage::FindEncoder
                    | Stage::CloseEncoder
                    | Stage::RestartCloseEncoder
                    | Stage::CpuFallbackCloseCapture
                    | Stage::FindCapture
                    | Stage::CloseCapture
                    | Stage::StopCapture
            ))
    {
        if stream.stage == Stage::RestartCloseEncoder {
            stream.cleanup("encoder_restart_failed");
            return false;
        }
        if stream.stage == Stage::Encoder
            && begin_encoder_fallback(stream, payload_error_code(&payload))
        {
            return false;
        }
        let code = if stream.stage == Stage::SelectEncoder {
            encoder_selection_error(payload_error_code(&payload))
        } else if stream.stage == Stage::Encoder {
            encoder_failure_code(payload_error_code(&payload))
        } else if stream.stage == Stage::FallbackEnumerate {
            stream.fallback_error.unwrap_or("provider_failed")
        } else {
            "provider_failed"
        };
        // Status reports only the coarse code; the log names the stage and the
        // provider's own short code so a failed open can be diagnosed.
        eprintln!(
            "amaterasu video step failed stage={:?} provider={} reported={} code={code}",
            stream.stage,
            stream.capture_service(),
            owned_capture::retained_reason(payload_error_code(&payload).unwrap_or("none")),
        );
        failed_step(stream, code);
        return false;
    }
    let next = match stream.stage {
        Stage::SelectEncoder => {
            let prefer_dmabuf = stream.owned_capture.prefers_dmabuf(plane);
            let software_only = matches!(
                stream.requested_encoder,
                EncoderSelection::Backend(EncoderBackend::Software)
            );
            let mut candidates_payload = payload.clone();
            if software_only && let Some(entries) = candidates_payload["encoders"].as_array_mut() {
                entries.retain(|entry| entry["hardware"].as_bool() == Some(false));
            }
            match resolve_encoder_selection(
                stream.encoder_index,
                &stream.configuration,
                &candidates_payload,
                prefer_dmabuf,
            ) {
                Ok(choice) => {
                    stream.fallback_encoder = x264_fallback_for_auto(
                        stream.encoder_index,
                        &stream.configuration,
                        &choice.identity,
                        &payload,
                    );
                    stream
                        .owned_capture
                        .record_encoder_inputs(choice.accepts_dmabuf);
                    let probe_auto = matches!(
                        stream.requested_encoder,
                        EncoderSelection::Backend(EncoderBackend::Auto)
                    );
                    if probe_auto {
                        stream.encoder_candidates.push_back(choice);
                        while let Some(last) = stream.encoder_candidates.back() {
                            if let Some(entries) = candidates_payload["encoders"].as_array_mut() {
                                entries.retain(|entry| {
                                    serde_json::from_value::<EncoderIdentity>(
                                        entry["identity"].clone(),
                                    )
                                    .is_ok_and(|identity| identity != last.identity)
                                });
                            }
                            match resolve_encoder_selection(
                                None,
                                &stream.configuration,
                                &candidates_payload,
                                prefer_dmabuf,
                            ) {
                                Ok(next) => stream.encoder_candidates.push_back(next),
                                Err(_) => break,
                            }
                        }
                        begin_encoder_probe(stream);
                        return false;
                    }
                    stream.encoder = Some(choice.identity);
                    Stage::Capture
                }
                Err(code) => {
                    let code = if software_only {
                        match payload["software_encoder"]["reason"].as_str() {
                            Some("x264_feature_disabled") => "x264_feature_disabled",
                            Some("x264_headers_unavailable") => "x264_headers_unavailable",
                            Some("x264_runtime_unavailable") => "x264_runtime_unavailable",
                            _ => "software_encoder_unavailable",
                        }
                    } else {
                        code
                    };
                    stream.cleanup(code);
                    return false;
                }
            }
        }
        Stage::Capture => {
            let Some(capture) = payload.get("stream").and_then(Value::as_u64) else {
                stream.cleanup("invalid_capture_reply");
                return false;
            };
            stream.capture = Some(capture);
            let geometry = match capture_geometry(&payload) {
                Ok(geometry) => geometry,
                Err(code) => {
                    stream.cleanup(code);
                    return false;
                }
            };
            stream.desktop_geometry = geometry;
            stream.touch_port = match capture_touch_port(
                &payload,
                &stream.configuration,
                stream.desktop_geometry,
            ) {
                Ok(touch_port) => touch_port,
                Err(code) => {
                    stream.cleanup(code);
                    return false;
                }
            };
            if stream.session.is_some() {
                // A CPU capture fallback reopened capture under a running encoder.
                match verify_stream_geometry(plane, stream) {
                    Ok(GeometryProgress::Ready) => activate_or_resume(stream, id, queues, send),
                    Ok(GeometryProgress::Busy) => stream.stage = Stage::Activating,
                    Err(code) => stream.cleanup(code),
                }
                return false;
            }
            Stage::Encoder
        }
        Stage::FallbackEnumerate => {
            let Some(device_name) = stream.fallback_device_name.clone() else {
                stream.cleanup(stream.fallback_error.unwrap_or("provider_failed"));
                return false;
            };
            let Some(provider) = stream.fallback_provider else {
                stream.cleanup(stream.fallback_error.unwrap_or("provider_failed"));
                return false;
            };
            let target_output =
                match accumulate_fallback_page(stream, &payload, provider, &device_name) {
                    Ok(Some(output)) => output,
                    Ok(None) => return false,
                    Err(()) => {
                        stream.cleanup(stream.fallback_error.unwrap_or("provider_failed"));
                        return false;
                    }
                };
            stream.provider = provider;
            stream.capture_output = target_output;
            stream.capture_operation = format!("{}-capture-1", stream.operation);
            stream.capture_attempt = 1;
            Stage::Capture
        }
        Stage::Encoder => {
            if !accept_encoder(stream, &payload) {
                return false;
            }
            match verify_stream_geometry(plane, stream) {
                Ok(GeometryProgress::Ready) => activate_or_resume(stream, id, queues, send),
                Ok(GeometryProgress::Busy) => stream.stage = Stage::Activating,
                Err(code) => stream.cleanup(code),
            }
            return false;
        }
        Stage::FindEncoder => {
            if missing {
                Stage::FindCapture
            } else {
                let Some(session) = payload.get("session_id").and_then(Value::as_u64) else {
                    failed_step(stream, "invalid_lookup");
                    return false;
                };
                stream.session = Some(session);
                Stage::CloseEncoder
            }
        }
        Stage::CloseEncoder => Stage::FindCapture,
        Stage::RestartCloseEncoder => {
            stream.session = None;
            let Some(fallback) = stream.fallback_encoder.take() else {
                stream.cleanup("encoder_restart_unavailable");
                return false;
            };
            stream.encoder = Some(fallback);
            stream.encoder_attempt = 1;
            stream.retries = 0;
            Stage::Encoder
        }
        Stage::FindCapture => {
            if missing {
                return finish_cleanup(stream);
            } else {
                let Some(capture) = payload.get("stream").and_then(Value::as_u64) else {
                    failed_step(stream, "invalid_lookup");
                    return false;
                };
                stream.capture = Some(capture);
                Stage::CloseCapture
            }
        }
        Stage::CpuFallbackCloseCapture => {
            owned_capture::reopen_cpu_capture(stream);
            return false;
        }
        Stage::CloseCapture => return finish_cleanup(stream),
        Stage::StopCapture => {
            stream.capture = None;
            Stage::Draining
        }
        _ => unreachable!("result belongs to submitted control stage"),
    };
    stream.stage = next;
    stream.retries = 0;
    false
}

fn begin_encoder_probe(stream: &mut Stream) -> bool {
    let Some(choice) = stream.encoder_candidates.pop_front() else {
        return false;
    };
    stream
        .owned_capture
        .record_encoder_inputs(choice.accepts_dmabuf);
    stream.encoder = Some(choice.identity);
    stream.encoder_probe_attempt += 1;
    stream.stage = Stage::ProbeEncoder;
    stream.retries = 0;
    true
}

#[derive(Clone, Debug, Deserialize)]
struct CaptureOutputIdentity {
    id: String,
    device_name: String,
    attached: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct X11OutputIdentity {
    id: String,
    adapter: String,
    adapter_vendor_id: Option<u32>,
    adapter_device_id: Option<u32>,
    adapter_description: Option<String>,
    device_name: String,
    attached: bool,
    left: i32,
    top: i32,
    width: u32,
    height: u32,
    rotation: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KmsOutputIdentity {
    id: String,
    adapter: String,
    adapter_vendor_id: Option<u32>,
    adapter_device_id: Option<u32>,
    adapter_description: Option<String>,
    device_name: String,
    attached: bool,
    framebuffer_export_supported: bool,
    readback_reason: Option<String>,
    left: i32,
    top: i32,
    width: u32,
    height: u32,
    rotation: u32,
}

fn x11_selection_output(
    payload: &Value,
    selected_id: &str,
    configuration: &Configuration,
) -> Result<(), &'static str> {
    if payload.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err("capture_provider_unavailable");
    }
    if payload
        .get("next_offset")
        .is_some_and(|value| !value.is_null())
    {
        return Err("invalid_capture_selection");
    }
    let outputs = payload
        .get("outputs")
        .and_then(Value::as_array)
        .filter(|outputs| outputs.len() <= 64)
        .ok_or("invalid_capture_selection")?;
    let selected: Vec<_> = outputs
        .iter()
        .filter(|output| output.get("id").and_then(Value::as_str) == Some(selected_id))
        .collect();
    let selected = match selected.as_slice() {
        [] => return Err("capture_output_unavailable"),
        [selected] => selected,
        _ => return Err("invalid_capture_selection"),
    };
    let output: X11OutputIdentity =
        serde_json::from_value((*selected).clone()).map_err(|_| "invalid_capture_selection")?;
    if output.id != selected_id
        || output.adapter != "x11"
        || output.adapter_vendor_id.is_some()
        || output.adapter_device_id.is_some()
        || output.adapter_description.is_some()
        || output.device_name.is_empty()
        || output.device_name.len() > 127
        || output.device_name.chars().any(char::is_control)
        || !output.attached
        || output.left < 0
        || output.top < 0
        || output.rotation != 1
        || output.width != configuration.width
        || output.height != configuration.height
    {
        return Err(
            if output.width != configuration.width || output.height != configuration.height {
                "capture_dimensions_mismatch"
            } else {
                "invalid_capture_selection"
            },
        );
    }
    Ok(())
}

fn kms_selection_output(
    payload: &Value,
    selected_id: &str,
    configuration: &Configuration,
) -> Result<(), &'static str> {
    if payload.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err("capture_provider_unavailable");
    }
    if payload
        .get("next_offset")
        .is_some_and(|value| !value.is_null())
    {
        return Err("invalid_capture_selection");
    }
    let outputs = payload
        .get("outputs")
        .and_then(Value::as_array)
        .filter(|outputs| outputs.len() <= 64)
        .ok_or("invalid_capture_selection")?;
    let selected: Vec<_> = outputs
        .iter()
        .filter(|output| output.get("id").and_then(Value::as_str) == Some(selected_id))
        .collect();
    let selected = match selected.as_slice() {
        [] => return Err("capture_output_unavailable"),
        [selected] => selected,
        _ => return Err("invalid_capture_selection"),
    };
    let output: KmsOutputIdentity =
        serde_json::from_value((*selected).clone()).map_err(|_| "invalid_capture_selection")?;
    if output.id != selected_id
        || !valid_kms_output_id(&output.id)
        || output.adapter
            != format!(
                "/dev/dri/{}",
                output.id[4..].split('/').next().unwrap_or("")
            )
        || output.adapter_vendor_id.is_some()
        || output.adapter_device_id.is_some()
        || output.adapter_description.as_deref() != Some("DRM/KMS primary plane")
        || output.device_name.is_empty()
        || output.device_name.len() > 127
        || output.device_name.chars().any(char::is_control)
        || !output.attached
        || !output.framebuffer_export_supported
        || output.readback_reason.is_some()
        || output.left < 0
        || output.top < 0
        || output.rotation != 1
        || output.width != configuration.width
        || output.height != configuration.height
    {
        return Err(
            if output.width != configuration.width || output.height != configuration.height {
                "capture_dimensions_mismatch"
            } else {
                "capture_output_unavailable"
            },
        );
    }
    Ok(())
}

fn valid_kms_output_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("kms:") else {
        return false;
    };
    let Some((card, identity)) = rest.split_once('/') else {
        return false;
    };
    let Some(card_number) = card.strip_prefix("card") else {
        return false;
    };
    if card_number.is_empty()
        || !card_number.bytes().all(|byte| byte.is_ascii_digit())
        || (card_number.len() > 1 && card_number.starts_with('0'))
        || card_number.parse::<u32>().is_err()
    {
        return false;
    }
    let Some((connector, identity)) = identity
        .strip_prefix("conn=")
        .and_then(|rest| rest.split_once("/crtc="))
    else {
        return false;
    };
    let Some((crtc, plane)) = identity.split_once("/plane=") else {
        return false;
    };
    [connector, crtc, plane].into_iter().all(|value| {
        !value.is_empty()
            && value.bytes().all(|byte| byte.is_ascii_digit())
            && (value.len() == 1 || !value.starts_with('0'))
            && value.parse::<u32>().is_ok_and(|number| number > 0)
    })
}

fn alternate_provider(provider: CaptureProvider) -> Option<CaptureProvider> {
    match provider {
        CaptureProvider::Dxgi => Some(CaptureProvider::Wgc),
        CaptureProvider::Wgc => Some(CaptureProvider::Dxgi),
        // Owned-output vkms fallback is handled separately, before activation.
        CaptureProvider::Pipewire | CaptureProvider::Vkms => None,
        CaptureProvider::X11 | CaptureProvider::Kms => None,
    }
}

fn begin_fallback(stream: &mut Stream, original_error: &'static str) -> bool {
    let Some(provider) = stream.fallback_provider else {
        return false;
    };
    if !stream.auto_capture || stream.constant_rate || stream.fallback_attempted {
        return false;
    }
    let Some(device_name) = output_device_name(stream.provider, &stream.output) else {
        return false;
    };
    stream.fallback_attempted = true;
    stream.no_resources = true;
    stream.fallback_error = Some(original_error);
    stream.fallback_device_name = Some(device_name);
    stream.fallback_offset = 0;
    stream.fallback_pages = 0;
    stream.fallback_last_output = None;
    stream.fallback_match = None;
    stream.fallback_enumeration_id = None;
    stream.stage = Stage::FallbackEnumerate;
    stream.retries = 0;
    debug_assert_ne!(stream.provider, provider);
    true
}

fn payload_error_code(payload: &Value) -> Option<&str> {
    payload
        .get("code")
        .or_else(|| payload.get("error").and_then(|error| error.get("code")))
        .and_then(Value::as_str)
}

const MAX_ENCODER_ENTRIES: usize = 64;
const SOFTWARE_ENCODER_ID: &str = "amaterasu.software.x264";
const SOFTWARE_HEVC_ENCODER_ID: &str = "amaterasu.software.linux.x265.hevc";
const LINUX_FFMPEG_NVENC_ID: &str = "amaterasu.hardware.nvidia.nvenc.ffmpeg";
const LINUX_FFMPEG_VAAPI_ID: &str = "amaterasu.hardware.vaapi.h264.ffmpeg";
/// Native NVENC sessions, one identity per codec: `<prefix>h264|hevc|av1`.
const NVENC_CUDA_ENCODER_PREFIX: &str = "amaterasu.hardware.nvenc-cuda.";

/// The encoder chosen from an enumeration, with whether its description lists
/// DMA-BUF among the frame inputs it accepts.
struct EncoderChoice {
    identity: EncoderIdentity,
    accepts_dmabuf: bool,
}

/// `inputs` is optional and parsed tolerantly: an absent or malformed list
/// means the legacy CPU-only input, and unknown input kinds are ignored.
fn encoder_accepts_dmabuf(entry: &Value) -> bool {
    entry
        .get("inputs")
        .and_then(Value::as_array)
        .is_some_and(|inputs| inputs.iter().any(|input| input.as_str() == Some("dmabuf")))
}

fn x264_fallback_for_auto(
    index: Option<u32>,
    configuration: &Configuration,
    selected: &EncoderIdentity,
    payload: &Value,
) -> Option<EncoderIdentity> {
    // x264 encodes only H.264; HEVC and AV1 never gain a software fallback here.
    if index.is_some()
        || configuration.codec != VideoCodec::H264
        || !(matches!(
            selected.clsid.as_str(),
            LINUX_FFMPEG_NVENC_ID | LINUX_FFMPEG_VAAPI_ID
        ) || selected.clsid.starts_with(NVENC_CUDA_ENCODER_PREFIX))
    {
        return None;
    }
    payload
        .get("encoders")?
        .as_array()?
        .iter()
        .find_map(|entry| {
            let identity: EncoderIdentity =
                serde_json::from_value(entry.get("identity")?.clone()).ok()?;
            (identity.clsid == SOFTWARE_ENCODER_ID
                && entry.get("hardware")?.as_bool() == Some(false))
            .then_some(identity)
        })
}

fn begin_encoder_fallback(stream: &mut Stream, reason: Option<&str>) -> bool {
    let confirmed_open_failure = match stream
        .encoder
        .as_ref()
        .map(|encoder| encoder.clsid.as_str())
    {
        Some(LINUX_FFMPEG_NVENC_ID) => matches!(
            reason,
            Some(
                "nvenc_device_unavailable"
                    | "nvenc_allocation_failed"
                    | "nvenc_open_failed"
                    | "nvenc_configuration_unsupported"
                    | "nvenc_headers_unavailable"
                    | "nvenc_headers_unsupported"
            )
        ),
        Some(clsid) if clsid.starts_with(NVENC_CUDA_ENCODER_PREFIX) => matches!(
            reason,
            Some(
                "nvenc_cuda_library_unavailable"
                    | "nvenc_cuda_driver_unavailable"
                    | "nvenc_cuda_api_unsupported"
                    | "nvenc_cuda_driver_too_old"
                    | "nvenc_cuda_device_unavailable"
                    | "nvenc_cuda_configuration_unsupported"
                    | "nvenc_cuda_converter_unavailable"
                    | "nvenc_cuda_allocation_failed"
                    | "nvenc_cuda_headers_unavailable"
                    | "nvenc_cuda_feature_disabled"
            )
        ),
        Some(LINUX_FFMPEG_VAAPI_ID) => matches!(
            reason,
            Some(
                "vaapi_device_unavailable"
                    | "vaapi_allocation_failed"
                    | "vaapi_open_failed"
                    | "vaapi_configuration_unsupported"
                    | "vaapi_headers_unavailable"
                    | "vaapi_headers_unsupported"
            )
        ),
        _ => false,
    };
    if !confirmed_open_failure || stream.encoder_attempt != 0 {
        return false;
    }
    let Some(fallback) = stream.fallback_encoder.take() else {
        return false;
    };
    stream.encoder = Some(fallback);
    stream.encoder_attempt = 1;
    stream.retries = 0;
    true
}

fn encoder_supports_configuration(
    identity: &EncoderIdentity,
    configuration: &Configuration,
) -> bool {
    match identity.clsid.as_str() {
        SOFTWARE_HEVC_ENCODER_ID => {
            configuration.codec == VideoCodec::Hevc
                && matches!(
                    configuration.color_mode,
                    ColorMode::Bt709Limited | ColorMode::Bt709Full
                )
                && configuration.max_reference_frames <= 1
        }
        LINUX_FFMPEG_NVENC_ID => {
            configuration.codec == VideoCodec::H264
                && matches!(
                    configuration.color_mode,
                    ColorMode::Bt709Limited | ColorMode::Bt709Full
                )
                && configuration.max_reference_frames <= 1
        }
        LINUX_FFMPEG_VAAPI_ID => {
            configuration.codec == VideoCodec::H264
                && configuration.color_mode == ColorMode::Bt709Limited
                && configuration.max_reference_frames == 0
        }
        SOFTWARE_ENCODER_ID => configuration.codec == VideoCodec::H264,
        // The CUDA backend converts every SDR color mode on the GPU and
        // signals it in the bitstream; each clsid encodes exactly one codec.
        clsid if clsid.starts_with(NVENC_CUDA_ENCODER_PREFIX) => {
            clsid.strip_prefix(NVENC_CUDA_ENCODER_PREFIX) == Some(codec_name(configuration.codec))
                && (configuration.codec != VideoCodec::H264
                    || configuration.max_reference_frames <= 1)
        }
        _ => configuration.codec == VideoCodec::H264,
    }
}

/// Resolve profile indices against the current bounded codec registry before
/// capture is opened. `None` means explicit `auto`: hardware first, then the
/// registered software encoder. With `prefer_dmabuf`, auto ranks hardware that
/// accepts DMA-BUF input ahead of other hardware. AV1 has no software encoder,
/// so it never falls back past hardware. No missing/out-of-range selection is
/// guessed.
fn resolve_encoder_selection(
    index: Option<u32>,
    configuration: &Configuration,
    payload: &Value,
    prefer_dmabuf: bool,
) -> Result<EncoderChoice, &'static str> {
    if payload.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(encoder_selection_error(payload_error_code(payload)));
    }
    if payload.get("enabled").and_then(Value::as_bool) != Some(true) {
        return Err("encoder_disabled");
    }
    let entries = payload
        .get("encoders")
        .and_then(Value::as_array)
        .ok_or("invalid_encoder_enumeration")?;
    if entries.len() > MAX_ENCODER_ENTRIES {
        return Err("invalid_encoder_enumeration");
    }
    if entries.is_empty() {
        return Err(codec_unavailable(configuration.codec));
    }

    let mut identities = Vec::with_capacity(entries.len());
    let mut hardware = Vec::new();
    let mut software = Vec::new();
    for entry in entries {
        let identity: EncoderIdentity = serde_json::from_value(
            entry
                .get("identity")
                .cloned()
                .ok_or("invalid_encoder_enumeration")?,
        )
        .map_err(|_| "invalid_encoder_enumeration")?;
        let codec = entry
            .get("codec")
            .and_then(Value::as_str)
            .filter(|codec| matches!(*codec, "h264" | "hevc" | "av1"))
            .ok_or("invalid_encoder_enumeration")?;
        if !valid_encoder_identity(&identity) {
            return Err("invalid_encoder_enumeration");
        }
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .ok_or("invalid_encoder_enumeration")?;
        if name.is_empty()
            || name.len() > 512
            || name.chars().any(char::is_control)
            || identities.contains(&identity)
        {
            return Err("invalid_encoder_enumeration");
        }
        let is_hardware = entry
            .get("hardware")
            .and_then(Value::as_bool)
            .ok_or("invalid_encoder_enumeration")?;
        if identity.clsid == SOFTWARE_ENCODER_ID && is_hardware {
            return Err("invalid_encoder_enumeration");
        }
        let accepts_dmabuf = encoder_accepts_dmabuf(entry);
        if is_hardware {
            if codec == codec_name(configuration.codec) {
                hardware.push((identity.clone(), accepts_dmabuf));
            }
        } else if identity.clsid == software_encoder_id(configuration.codec)
            && codec == codec_name(configuration.codec)
        {
            software.push((identity.clone(), accepts_dmabuf));
        }
        identities.push(identity);
    }

    if let Some(index) = index {
        let selected = identities
            .get(index as usize)
            .ok_or("encoder_unavailable")?;
        let selected_codec = entries[index as usize]
            .get("codec")
            .and_then(Value::as_str)
            .ok_or("invalid_encoder_enumeration")?;
        if selected_codec != codec_name(configuration.codec) {
            return Err(codec_unavailable(configuration.codec));
        }
        if !encoder_supports_configuration(selected, configuration) {
            return Err("encoder_configuration_unsupported");
        }
        return Ok(EncoderChoice {
            identity: selected.clone(),
            accepts_dmabuf: encoder_accepts_dmabuf(&entries[index as usize]),
        });
    }
    let supported = |(identity, _): &(EncoderIdentity, bool)| {
        encoder_supports_configuration(identity, configuration)
    };
    let dmabuf_hardware = hardware
        .iter()
        .filter(|candidate| prefer_dmabuf && candidate.1)
        .find(|candidate| supported(candidate));
    dmabuf_hardware
        .or_else(|| hardware.iter().find(|candidate| supported(candidate)))
        .or_else(|| software.iter().find(|candidate| supported(candidate)))
        .map(|(identity, accepts_dmabuf)| EncoderChoice {
            identity: identity.clone(),
            accepts_dmabuf: *accepts_dmabuf,
        })
        .ok_or_else(|| codec_unavailable(configuration.codec))
}

fn codec_name(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "h264",
        VideoCodec::Hevc => "hevc",
        VideoCodec::Av1 => "av1",
    }
}

fn software_encoder_id(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => SOFTWARE_ENCODER_ID,
        VideoCodec::Hevc => SOFTWARE_HEVC_ENCODER_ID,
        // There is no software AV1 backend; this id matches no encoder.
        VideoCodec::Av1 => "amaterasu.software.av1-unavailable",
    }
}

fn codec_unavailable(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "h264_encoder_unavailable",
        VideoCodec::Hevc => "hevc_encoder_unavailable",
        VideoCodec::Av1 => "av1_encoder_unavailable",
    }
}

fn valid_encoder_identity(identity: &EncoderIdentity) -> bool {
    let valid_text = |value: &str, maximum: usize| {
        !value.is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
    };
    valid_text(&identity.clsid, 128)
        && valid_text(&identity.friendly_name, 512)
        && identity
            .hardware_url
            .as_deref()
            .is_none_or(|value| valid_text(value, 2048))
}

fn encoder_selection_error(reason: Option<&str>) -> &'static str {
    match reason {
        Some("disabled") => "encoder_disabled",
        Some("unsupported_platform") => "encoder_unavailable",
        _ => "encoder_enumeration_failed",
    }
}

fn encoder_failure_code(reason: Option<&str>) -> &'static str {
    match reason {
        Some(
            "disabled"
            | "unsupported_platform"
            | "software_encoder_unavailable"
            | "encoder_identity_changed"
            | "x264_feature_disabled"
            | "x264_headers_unavailable"
            | "x264_runtime_unavailable",
        ) => "h264_encoder_unavailable",
        Some(
            "hevc_feature_disabled"
            | "hevc_configuration_unsupported"
            | "hevc_x265_abi_unsupported"
            | "hevc_allocation_failed"
            | "hevc_profile_unsupported"
            | "hevc_open_failed"
            | "hevc_headers_unavailable"
            | "hevc_headers_unsupported",
        ) => "hevc_encoder_unavailable",
        _ => "encoder_failed",
    }
}

#[cfg(test)]
mod integration_contract_tests {
    use super::*;

    fn select(
        index: Option<u32>,
        configuration: &Configuration,
        payload: &Value,
    ) -> Result<EncoderIdentity, &'static str> {
        resolve_encoder_selection(index, configuration, payload, false)
            .map(|choice| choice.identity)
    }

    fn default_configuration() -> Configuration {
        serde_json::from_value(json!({
            "width": 1920, "height": 1080, "fps": 60, "bitrate": 15_000_000
        }))
        .unwrap()
    }

    fn description(clsid: &str, name: &str, hardware: bool) -> Value {
        description_for_codec(clsid, name, hardware, "h264")
    }

    fn description_for_codec(clsid: &str, name: &str, hardware: bool, codec: &str) -> Value {
        json!({
            "identity":{"clsid":clsid,"hardware_url":null,"friendly_name":name},
            "name":name,
            "codec":codec,
            "hardware":hardware
        })
    }

    #[test]
    fn missing_linux_h264_backend_has_a_specific_pipeline_failure() {
        for reason in [
            "disabled",
            "unsupported_platform",
            "software_encoder_unavailable",
            "encoder_identity_changed",
            "x264_feature_disabled",
            "x264_headers_unavailable",
            "x264_runtime_unavailable",
        ] {
            assert_eq!(
                encoder_failure_code(Some(reason)),
                "h264_encoder_unavailable"
            );
        }
        assert_eq!(encoder_failure_code(Some("driver_error")), "encoder_failed");
    }

    #[test]
    fn registry_selection_preserves_ordinals_and_rejects_out_of_range() {
        let payload = json!({
            "ok":true,"enabled":true,"encoders":[
                description("first-software", "First software", false),
                description("second-hardware", "Second hardware", true)
            ]
        });
        assert_eq!(
            select(Some(0), &default_configuration(), &payload)
                .unwrap()
                .clsid,
            "first-software"
        );
        assert!(matches!(
            select(Some(2), &default_configuration(), &payload),
            Err("encoder_unavailable")
        ));
    }

    #[test]
    fn enumeration_rejects_malformed_names_and_duplicate_identities() {
        let overlong_name = "x".repeat(513);
        for malformed_name in ["", overlong_name.as_str(), "bad\nname"] {
            let payload = json!({
                "ok":true,"enabled":true,
                "encoders":[description("hardware", malformed_name, true)]
            });
            assert!(matches!(
                select(Some(0), &default_configuration(), &payload),
                Err("invalid_encoder_enumeration")
            ));
        }

        let duplicate = description("hardware", "Hardware", true);
        let payload = json!({
            "ok":true,"enabled":true,
            "encoders":[duplicate.clone(), duplicate]
        });
        assert!(matches!(
            select(None, &default_configuration(), &payload),
            Err("invalid_encoder_enumeration")
        ));
    }

    #[test]
    fn auto_uses_only_registered_x264_after_hardware_is_absent() {
        let arbitrary_software = json!({
            "ok":true,"enabled":true,
            "encoders":[description("other.software", "Other software", false)]
        });
        assert!(matches!(
            select(None, &default_configuration(), &arbitrary_software),
            Err("h264_encoder_unavailable")
        ));

        let x264 = json!({
            "ok":true,"enabled":true,
            "encoders":[description(SOFTWARE_ENCODER_ID, "x264", false)]
        });
        assert_eq!(
            select(None, &default_configuration(), &x264).unwrap().clsid,
            SOFTWARE_ENCODER_ID
        );
    }

    #[test]
    fn auto_falls_back_to_x264_when_nvenc_cannot_encode_selected_color() {
        let payload = json!({
            "ok": true, "enabled": true, "encoders": [
                description(LINUX_FFMPEG_NVENC_ID, "NVENC", true),
                description(SOFTWARE_ENCODER_ID, "x264", false)
            ]
        });
        let mut configuration = default_configuration();
        assert_eq!(
            select(None, &configuration, &payload).unwrap().clsid,
            LINUX_FFMPEG_NVENC_ID
        );
        configuration.color_mode = ColorMode::Bt709Full;
        assert_eq!(
            select(None, &configuration, &payload).unwrap().clsid,
            LINUX_FFMPEG_NVENC_ID
        );
        configuration.color_mode = ColorMode::Bt601Full;
        assert_eq!(
            select(None, &configuration, &payload).unwrap().clsid,
            SOFTWARE_ENCODER_ID
        );
        assert!(matches!(
            select(Some(0), &configuration, &payload),
            Err("encoder_configuration_unsupported")
        ));
        configuration.color_mode = ColorMode::Bt709Limited;
        configuration.max_reference_frames = 1;
        assert_eq!(
            select(None, &configuration, &payload).unwrap().clsid,
            LINUX_FFMPEG_NVENC_ID
        );
    }

    #[test]
    fn vaapi_is_auto_selected_only_for_its_supported_limited_color() {
        let payload = json!({
            "ok": true, "enabled": true, "encoders": [
                description(LINUX_FFMPEG_VAAPI_ID, "VAAPI", true),
                description(SOFTWARE_ENCODER_ID, "x264", false)
            ]
        });
        let mut configuration = default_configuration();
        assert_eq!(
            select(None, &configuration, &payload).unwrap().clsid,
            LINUX_FFMPEG_VAAPI_ID
        );
        configuration.color_mode = ColorMode::Bt709Full;
        assert_eq!(
            select(None, &configuration, &payload).unwrap().clsid,
            SOFTWARE_ENCODER_ID
        );
        assert!(matches!(
            select(Some(0), &configuration, &payload),
            Err("encoder_configuration_unsupported")
        ));
    }

    #[test]
    fn hevc_auto_selection_requires_registered_x265_identity_and_supported_bt709_range() {
        let payload = json!({
            "ok":true,"enabled":true,"encoders":[
                description(SOFTWARE_ENCODER_ID, "x264", false),
                description_for_codec(SOFTWARE_HEVC_ENCODER_ID, "x265 Main", false, "hevc")
            ]
        });
        let mut configuration = default_configuration();
        configuration.codec = VideoCodec::Hevc;
        assert_eq!(
            select(None, &configuration, &payload).unwrap().clsid,
            SOFTWARE_HEVC_ENCODER_ID
        );

        configuration.color_mode = ColorMode::Bt709Full;
        assert_eq!(
            select(None, &configuration, &payload).unwrap().clsid,
            SOFTWARE_HEVC_ENCODER_ID
        );

        configuration.color_mode = ColorMode::Bt601Full;
        assert!(matches!(
            select(None, &configuration, &payload),
            Err("hevc_encoder_unavailable")
        ));

        configuration.color_mode = ColorMode::Bt709Limited;
        let h264_only = json!({
            "ok":true,"enabled":true,
            "encoders":[description(SOFTWARE_ENCODER_ID, "x264", false)]
        });
        assert!(matches!(
            select(None, &configuration, &h264_only),
            Err("hevc_encoder_unavailable")
        ));
    }
}

fn output_device_name(provider: CaptureProvider, output_id: &str) -> Option<String> {
    match provider {
        CaptureProvider::Dxgi => {
            let (adapter_id, device_name) = output_id.split_once('/')?;
            let (high, low) = adapter_id.split_once(':')?;
            let luid_is_valid = high.len() == 8
                && low.len() == 8
                && high.bytes().all(|byte| byte.is_ascii_hexdigit())
                && low.bytes().all(|byte| byte.is_ascii_hexdigit());
            (luid_is_valid && !device_name.is_empty() && !device_name.contains('/'))
                .then(|| device_name.to_owned())
        }
        CaptureProvider::Wgc => {
            (!output_id.is_empty() && !output_id.contains('/')).then(|| output_id.to_owned())
        }
        // Portal IDs are scoped to a selected session and have no Windows alias.
        CaptureProvider::Pipewire | CaptureProvider::Vkms => None,
        CaptureProvider::X11 => output_id
            .strip_prefix("x11:")
            .and_then(|value| value.rsplit_once('/').map(|(name, _)| name.to_owned())),
        CaptureProvider::Kms => None,
    }
}

const MAX_FALLBACK_ENUMERATION_PAGES: usize = 64;

fn accumulate_fallback_page(
    stream: &mut Stream,
    payload: &Value,
    provider: CaptureProvider,
    device_name: &str,
) -> Result<Option<String>, ()> {
    let outputs: Vec<CaptureOutputIdentity> =
        serde_json::from_value(payload.get("outputs").ok_or(())?.clone()).map_err(|_| ())?;
    let next_offset = payload
        .get("next_offset")
        .map(|value| {
            value
                .as_u64()
                .and_then(|number| usize::try_from(number).ok())
                .ok_or(())
        })
        .transpose()?;
    if stream.fallback_pages >= MAX_FALLBACK_ENUMERATION_PAGES {
        return Err(());
    }
    let paged = stream.fallback_offset != 0 || next_offset.is_some();
    if paged {
        let enumeration_id = payload
            .get("enumeration_id")
            .and_then(Value::as_str)
            .filter(|token| token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or(())?;
        if let Some(previous) = stream.fallback_enumeration_id.as_deref() {
            if previous != enumeration_id {
                return Err(());
            }
        } else {
            stream.fallback_enumeration_id = Some(enumeration_id.to_owned());
        }
    }
    if paged && outputs.is_empty() {
        return Err(());
    }
    let expected_next = stream
        .fallback_offset
        .checked_add(outputs.len())
        .ok_or(())?;
    if next_offset.is_some_and(|next| next != expected_next) {
        return Err(());
    }
    for output in outputs {
        if paged {
            if stream
                .fallback_last_output
                .as_deref()
                .is_some_and(|last| output.id.as_str() <= last)
            {
                return Err(());
            }
            stream.fallback_last_output = Some(output.id.clone());
        }
        let matches = output.attached
            && output.device_name == device_name
            && match provider {
                CaptureProvider::Dxgi => {
                    output_device_name(CaptureProvider::Dxgi, &output.id).as_deref()
                        == Some(device_name)
                }
                CaptureProvider::Wgc => output.id == device_name,
                CaptureProvider::Pipewire | CaptureProvider::Vkms => false,
                CaptureProvider::X11 | CaptureProvider::Kms => false,
            };
        if matches && stream.fallback_match.replace(output.id).is_some() {
            return Err(());
        }
    }
    stream.fallback_pages += 1;
    if let Some(next) = next_offset {
        stream.fallback_offset = next;
        Ok(None)
    } else {
        stream.fallback_match.take().map(Some).ok_or(())
    }
}

fn capture_geometry(payload: &Value) -> Result<Option<CaptureGeometry>, &'static str> {
    let Some(value) = payload.get("desktop_geometry") else {
        return Ok(None);
    };
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|_| "invalid_capture_geometry")
}

fn capture_touch_port(
    payload: &Value,
    configuration: &Configuration,
    desktop_geometry: Option<CaptureGeometry>,
) -> Result<Option<Value>, &'static str> {
    let Some(value) = payload.get("touch_surface") else {
        return Ok(None);
    };
    let surface: TouchSurface =
        serde_json::from_value(value.clone()).map_err(|_| "invalid_capture_touch_surface")?;
    if !surface.validate() {
        return Err("invalid_capture_touch_surface");
    }
    // Absolute input also needs the verified desktop rectangle. The current
    // media plane accepts only unscaled frames of the requested size.
    if !desktop_geometry.is_some_and(|geometry| {
        surface.width == geometry.output.width && surface.height == geometry.output.height
    }) || surface.width != configuration.width
        || surface.height != configuration.height
    {
        return Ok(None);
    }
    Ok(Some(json!({
        "width": configuration.width,
        "height": configuration.height,
        "logical_width": surface.logical_width,
        "logical_height": surface.logical_height,
        "client_offset_x": 0.0_f32,
        "client_offset_y": 0.0_f32,
    })))
}

fn accept_encoder(stream: &mut Stream, payload: &Value) -> bool {
    if payload.get("codec").and_then(Value::as_str) != Some(codec_name(stream.configuration.codec))
    {
        stream.cleanup("encoder_codec_mismatch");
        return false;
    }
    if stream.packet_boundaries
        && payload.get("packet_boundaries").and_then(Value::as_bool) != Some(true)
    {
        stream.cleanup("packet_boundaries_unavailable");
        return false;
    }
    if stream.configuration.max_reference_frames != 0
        && payload.get("max_reference_frames").and_then(Value::as_u64)
            != Some(u64::from(stream.configuration.max_reference_frames))
    {
        stream.cleanup("reference_frame_constraint_unavailable");
        return false;
    }
    let Some(session) = payload.get("session_id").and_then(Value::as_u64) else {
        stream.cleanup("invalid_encoder_reply");
        return false;
    };
    stream.session = Some(session);
    true
}

fn activate(stream: &mut Stream, id: u64, queues: &Arc<Mutex<Queues>>, send: &mut Send<'_>) {
    let Ok(mut guard) = queues.lock() else {
        stream.cleanup("queue_failed");
        return;
    };
    guard.open(id, &stream.owner, stream.clock_origin_us);
    guard.set_geometry(id, stream.desktop_geometry);
    drop(guard);
    stream.stage = Stage::Running;
    if let Some(parent) = stream.parent.take()
        && !rpc::complete(send, parent, stream.response(id))
    {
        stream.ending_parent = Some(parent);
        stream.cleanup("completion_lost");
        if let Ok(mut queues) = queues.lock() {
            queues.close(id);
        }
    }
}

fn activate_or_resume(
    stream: &mut Stream,
    id: u64,
    queues: &Arc<Mutex<Queues>>,
    send: &mut Send<'_>,
) {
    if stream.restart_pending {
        stream.restart_pending = false;
        stream.stage = Stage::Running;
        stream.force_idr = true;
        stream.next_frame = Instant::now();
    } else {
        activate(stream, id, queues, send);
    }
}

fn finish_cleanup(stream: &mut Stream) -> bool {
    if stream.unresolved {
        stream.stage = Stage::Failed;
        false
    } else {
        true
    }
}

#[cfg(test)]
mod tests;
