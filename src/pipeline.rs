mod lifecycle;
mod operations;
mod output;
mod pacing;

use crate::{
    model::{
        CaptureProvider, Configuration, EncoderIdentity, OpenSpec, Operation, Settings, error,
        valid_minimum_fps_target,
    },
    plane::{ENCODE, FrameMetadata, GeometryProgress, OwnedFrame, Plane, Pump},
    queue::Queues,
    rpc::{self, Send},
};
use amaterasu_media::CaptureGeometry;
use amaterasu_module_sdk::{Command, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Capture,
    FallbackEnumerate,
    Encoder,
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
    auto_capture: bool,
    fallback_provider: Option<CaptureProvider>,
    fallback_device_name: Option<String>,
    fallback_error: Option<&'static str>,
    fallback_attempted: bool,
    constant_rate: bool,
    minimum_fps_target: f64,
    wgc_pacing_smoothing: bool,
    wgc_pacing: pacing::WgcPacing,
    force_idr: bool,
    grace: Option<output::Grace>,
    output: String,
    capture_output: String,
    encoder: EncoderIdentity,
    configuration: Configuration,
    parent: Option<u64>,
    ending_parent: Option<u64>,
    pending: Option<u64>,
    unsettled: Option<u64>,
    uncertain: Option<Command>,
    settlement_attempts: u8,
    capture: Option<u64>,
    desktop_geometry: Option<CaptureGeometry>,
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
    last_output_at: Option<Instant>,
    last_repeat_attempt_at: Option<Instant>,
    last_output_timestamp_us: Option<u64>,
    last_output_sequence: u64,
    last_pixels: Option<OwnedFrame>,
    dropped: u64,
    submitted: u64,
    last_frame: Option<FrameMetadata>,
    unresolved: bool,
}

impl Stream {
    fn repeat_due(&self, now: Instant) -> bool {
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

    fn next_frame_clock(&self, now: Instant) -> Option<(u64, u64)> {
        let sequence = self.last_output_sequence.checked_add(1)?;
        let elapsed = now
            .saturating_duration_since(self.opened_at)
            .as_micros()
            .min(u64::MAX as u128) as u64;
        let timestamp_us = self
            .last_output_timestamp_us
            .map(|timestamp| timestamp.checked_add(1))
            .unwrap_or(Some(1))?
            .max(elapsed);
        Some((timestamp_us, sequence))
    }

    fn response(&self, id: u64) -> Value {
        let state = match self.stage {
            Stage::Capture | Stage::FallbackEnumerate | Stage::Encoder | Stage::Activating => {
                "opening"
            }
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
            "packet_boundaries": self.packet_boundaries,
            "supports_packet_boundaries": true,
            "output": self.output,
            "capture_provider":self.provider,
            "constant_rate": self.constant_rate,
            "wgc_pacing_smoothing": self.wgc_pacing_smoothing,
            "minimum_fps_target": self.minimum_fps_target,
            "force_idr_pending":self.force_idr,
            "configuration": self.configuration,
            "dropped_frames": self.dropped,
            "submitted_frames": self.submitted,
            "error": self.error,
        });
        if let Some(geometry) = self.desktop_geometry {
            response["desktop_geometry"] = json!(geometry);
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
    drain_timeout: Duration,
    recent: VecDeque<(String, Value)>,
    epoch: String,
    next_id: u64,
    cursor: usize,
    streams: BTreeMap<u64, Stream>,
    operations: BTreeMap<(String, String), operations::Record>,
    pub queues: Arc<Mutex<Queues>>,
}

impl Pipeline {
    pub fn new(settings: Settings, epoch: String, queues: Arc<Mutex<Queues>>) -> Self {
        Self {
            enabled: settings.enabled,
            providers: settings.capture_providers,
            drain_timeout: Duration::from_millis(settings.drain_timeout_ms),
            recent: VecDeque::new(),
            epoch,
            next_id: 0,
            cursor: 0,
            streams: BTreeMap::new(),
            operations: BTreeMap::new(),
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
                    configuration,
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
                    Stage::Capture
                        | Stage::FallbackEnumerate
                        | Stage::Encoder
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

    fn open(&mut self, caller: &str, request: u64, specification: OpenSpec, send: &mut Send<'_>) {
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
        let failure = if !self.enabled {
            Some("disabled")
        } else if !self.providers.contains(&provider) {
            Some("capture_provider_not_allowed")
        } else if constant_rate && provider != CaptureProvider::Wgc {
            Some("constant_rate_requires_wgc")
        } else if !valid_minimum_fps_target(minimum_fps_target)
            || !configuration.validate()
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
            let wgc_pacing = pacing::WgcPacing::new(wgc_pacing_smoothing, configuration.fps);
            let operation = format!("video-{}-{id}", self.epoch);
            let fallback_provider = if auto_capture && !constant_rate {
                alternate_provider(provider).filter(|candidate| self.providers.contains(candidate))
            } else {
                None
            };
            Stream {
                packet_boundaries,
                owner: caller.into(),
                provider,
                auto_capture,
                fallback_provider,
                fallback_device_name: None,
                fallback_error: None,
                fallback_attempted: false,
                constant_rate,
                minimum_fps_target,
                wgc_pacing_smoothing,
                wgc_pacing,
                force_idr: false,
                grace: None,
                capture_output: output.clone(),
                output,
                encoder,
                configuration,
                parent: Some(request),
                ending_parent: None,
                pending: None,
                unsettled: None,
                uncertain: None,
                settlement_attempts: 0,
                capture: None,
                desktop_geometry: None,
                session: None,
                capture_operation: format!("{operation}-capture-0"),
                operation,
                capture_attempt: 0,
                no_resources: false,
                stage: Stage::Capture,
                retries: 0,
                error: None,
                next_frame: Instant::now(),
                opened_at,
                last_output_at: None,
                last_repeat_attempt_at: None,
                last_output_timestamp_us: None,
                last_output_sequence: 0,
                last_pixels: None,
                dropped: 0,
                submitted: 0,
                last_frame: None,
                unresolved: false,
            }
        });
    }

    pub fn tick(&mut self, plane: &mut dyn Plane, send: &mut Send<'_>) {
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
        if matches!(stream.stage, Stage::StopCapture | Stage::Draining) {
            output::tick(stream, id, &self.queues, plane);
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
            pump(plane, stream, id);
            if stream.stage != Stage::Running
                && let Ok(mut queues) = self.queues.lock()
            {
                queues.close(id);
            }
            return;
        }
        if stream.stage == Stage::Activating {
            match plane.verify_geometry(
                stream.provider,
                stream
                    .capture
                    .expect("activation follows capture admission"),
                stream.desktop_geometry,
            ) {
                Ok(GeometryProgress::Ready) => activate(stream, id, &self.queues, send),
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
}

fn elapsed_at_rate(now: Instant, anchor: Instant, frames_per_second: f64) -> bool {
    now.saturating_duration_since(anchor).as_secs_f64() * frames_per_second >= 1.0
}

fn pump(plane: &mut dyn Plane, stream: &mut Stream, id: u64) {
    let now = Instant::now();
    let mut capture_due = now >= stream.next_frame;
    let mut capture_slot = capture_due.then_some(stream.next_frame);
    if capture_due {
        match plane.verify_geometry(
            stream.provider,
            stream.capture.expect("running stream owns capture"),
            stream.desktop_geometry,
        ) {
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
                capture_due = false;
                capture_slot = None;
            }
            Err(code) => {
                stream.cleanup(code);
                return;
            }
        }
        if capture_due && (stream.provider != CaptureProvider::Wgc || stream.constant_rate) {
            stream.next_frame = next_capture_deadline(now, stream.configuration.fps);
        }
    }
    let repeat_due = stream.repeat_due(now);
    let Some((timestamp_us, sequence)) = stream.next_frame_clock(now) else {
        stream.cleanup("frame_clock_exhausted");
        return;
    };
    match plane.pump(Pump {
        video: id,
        provider: stream.provider,
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
            let completed_at = Instant::now();
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
        Err(code) => stream.cleanup(code),
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
        if result.is_err() {
            stream.unsettled = Some(request);
        }
        stream.pending = None;
        return apply_result(stream, id, queues, plane, result, send);
    }
    if stream.stage == Stage::Encoder {
        match plane.verify_geometry(
            stream.provider,
            stream
                .capture
                .expect("encoder stage follows capture admission"),
            stream.desktop_geometry,
        ) {
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
        Stage::Capture | Stage::FallbackEnumerate | Stage::Encoder
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
            if stream.stage == Stage::Capture {
                stream.no_resources = true;
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
            }
            stream.uncertain = Some(command);
            failed_step(stream, "request_failed");
        }
    }
    false
}

fn request(stream: &Stream) -> (&'static str, Value) {
    let operation = &stream.operation;
    match stream.stage {
        Stage::Capture => {
            let mut request = json!({
                "action":"open",
                "output":stream.capture_output,
                "operation_id":stream.capture_operation,
            });
            if stream.constant_rate {
                request["constant_rate"] = json!(true);
            }
            (stream.provider.service(), request)
        }
        Stage::FallbackEnumerate => (
            stream
                .fallback_provider
                .expect("fallback enumeration has a provider")
                .service(),
            json!({"action":"enumerate"}),
        ),
        Stage::Encoder => (ENCODE, encoder_open_request(stream)),
        Stage::FindEncoder => (ENCODE, json!({"action":"lookup","operation_id":operation})),
        Stage::CloseEncoder => (
            ENCODE,
            json!({"action":"close","session_id":stream.session}),
        ),
        Stage::FindCapture => (
            stream.provider.service(),
            json!({"action":"lookup","operation_id":stream.capture_operation}),
        ),
        Stage::CloseCapture | Stage::StopCapture => (
            stream.provider.service(),
            json!({"action":"close","stream":stream.capture}),
        ),
        _ => unreachable!("only pending control stages submit"),
    }
}

fn encoder_open_request(stream: &Stream) -> Value {
    let mut request = json!({
        "action": "open",
        "encoder": stream.encoder,
        "configuration": stream.configuration,
        "operation_id": stream.operation,
    });
    if stream.packet_boundaries {
        request["packet_boundaries"] = json!(true);
    }
    request
}

fn stage_name(stage: Stage) -> &'static str {
    match stage {
        Stage::Capture => "capture",
        Stage::FallbackEnumerate => "fallback-enumerate",
        Stage::Encoder => "encoder",
        Stage::Activating => "activating",
        Stage::FindEncoder => "find-encoder",
        Stage::CloseEncoder => "close-encoder",
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
        Stage::Capture
            | Stage::FallbackEnumerate
            | Stage::Encoder
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
            failed_step(stream, code);
            return false;
        }
    };
    let success = payload.get("ok").and_then(Value::as_bool) == Some(true);
    let missing = rpc::missing(&payload);
    if stream.stage == Stage::Capture && payload.get("ok").and_then(Value::as_bool) == Some(false) {
        if payload_error_code(&payload) == Some("unsupported_platform") {
            stream.no_resources = true;
            if !begin_fallback(stream, "unsupported_platform") {
                failed_step(stream, "unsupported_platform");
            }
            return false;
        }
    }
    if !success
        && !(missing
            && matches!(
                stream.stage,
                Stage::FindEncoder
                    | Stage::CloseEncoder
                    | Stage::FindCapture
                    | Stage::CloseCapture
                    | Stage::StopCapture
            ))
    {
        let code = if stream.stage == Stage::FallbackEnumerate {
            stream.fallback_error.unwrap_or("provider_failed")
        } else {
            "provider_failed"
        };
        failed_step(stream, code);
        return false;
    }
    let next = match stream.stage {
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
            Stage::Encoder
        }
        Stage::FallbackEnumerate => {
            let Some(device_name) = stream.fallback_device_name.as_deref() else {
                stream.cleanup(stream.fallback_error.unwrap_or("provider_failed"));
                return false;
            };
            let Some(provider) = stream.fallback_provider else {
                stream.cleanup(stream.fallback_error.unwrap_or("provider_failed"));
                return false;
            };
            let Some(target_output) = matching_output(&payload, provider, device_name) else {
                stream.cleanup(stream.fallback_error.unwrap_or("provider_failed"));
                return false;
            };
            stream.provider = provider;
            stream.capture_output = target_output.id;
            stream.capture_operation = format!("{}-capture-1", stream.operation);
            stream.capture_attempt = 1;
            Stage::Capture
        }
        Stage::Encoder => {
            if !accept_encoder(stream, &payload) {
                return false;
            }
            match plane.verify_geometry(
                stream.provider,
                stream.capture.expect("encoder follows capture admission"),
                stream.desktop_geometry,
            ) {
                Ok(GeometryProgress::Ready) => activate(stream, id, queues, send),
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

#[derive(Clone, Debug, Deserialize)]
struct CaptureOutputIdentity {
    id: String,
    device_name: String,
    attached: bool,
}

fn alternate_provider(provider: CaptureProvider) -> Option<CaptureProvider> {
    match provider {
        CaptureProvider::Dxgi => Some(CaptureProvider::Wgc),
        CaptureProvider::Wgc => Some(CaptureProvider::Dxgi),
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
    }
}

fn matching_output(
    payload: &Value,
    provider: CaptureProvider,
    device_name: &str,
) -> Option<CaptureOutputIdentity> {
    let outputs: Vec<CaptureOutputIdentity> =
        serde_json::from_value(payload.get("outputs")?.clone()).ok()?;
    let mut matching = outputs.into_iter().filter(|output| {
        output.attached
            && output.device_name == device_name
            && match provider {
                CaptureProvider::Dxgi => {
                    output_device_name(CaptureProvider::Dxgi, &output.id).as_deref()
                        == Some(device_name)
                }
                CaptureProvider::Wgc => output.id == device_name,
            }
    });
    let output = matching.next()?;
    matching.next().is_none().then_some(output)
}

fn capture_geometry(payload: &Value) -> Result<Option<CaptureGeometry>, &'static str> {
    let Some(value) = payload.get("desktop_geometry") else {
        return Ok(None);
    };
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|_| "invalid_capture_geometry")
}

fn accept_encoder(stream: &mut Stream, payload: &Value) -> bool {
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
    guard.open(id, &stream.owner);
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
