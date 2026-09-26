use super::*;
use crate::model::{AmdAv1LatencyMode, EncoderIdentity};
use crate::plane::{CaptureArrival, FrameMetadata, GeometryProgress, OwnedFrame, Progress};
use amaterasu_media::{CaptureGeometry, DesktopRect};
const CAPTURE: &str = "amaterasu.capture-dxgi/1";
#[derive(Default)]
struct Runtime {
    next: u64,
    operations: BTreeMap<String, u64>,
    ended: std::collections::BTreeSet<u64>,
    cancel_failures: usize,
    release_failures: usize,
    parent_failures: usize,
    open_effects: usize,
    pending: BTreeMap<u64, Value>,
    commands: Vec<Command>,
    completed: Vec<(u64, Value)>,
    cancelled: bool,
    lost_complete: bool,
    fail_encoder_cleanup: bool,
    lose_accept: bool,
    lose_result: bool,
    omit_reference_metadata: bool,
    capture_geometry: Option<Value>,
    capture_open_errors: BTreeMap<CaptureProvider, &'static str>,
    capture_admission_errors: BTreeMap<CaptureProvider, &'static str>,
    capture_outputs: BTreeMap<CaptureProvider, Vec<Value>>,
}

impl Runtime {
    fn send(&mut self, command: Command) -> amaterasu_module_sdk::ModuleResult<Response> {
        self.commands.push(command.clone());
        Ok(match command {
            Command::CheckRequest { .. } => {
                if self.cancelled {
                    Response::error("cancelled", "")
                } else {
                    Response::Ok
                }
            }
            Command::Request {
                service,
                payload,
                operation_id,
                parent,
                ..
            } => {
                if let Some(id) = self.operations.get(&operation_id) {
                    return Ok(Response::Accepted { id: *id });
                }
                if parent.is_some_and(|parent| self.ended.contains(&parent)) {
                    return Ok(Response::error("permission_denied", "parent ended"));
                }
                let action = payload["action"].as_str().unwrap();
                let capture_provider = capture_provider_for_service(&service);
                if action == "open"
                    && let Some(provider) = capture_provider
                    && let Some(code) = self.capture_admission_errors.get(&provider)
                {
                    return Ok(Response::error(code, ""));
                }
                self.next += 1;
                self.operations.insert(operation_id, self.next);
                if action == "open" {
                    self.open_effects += 1;
                }
                let result = if self.fail_encoder_cleanup
                    && service == ENCODE
                    && matches!(action, "lookup" | "close")
                {
                    error("hardware_error")
                } else if let Some(provider) = capture_provider {
                    if action == "enumerate" {
                        json!({
                            "ok":true,
                            "outputs":self.capture_outputs.get(&provider).cloned().unwrap_or_default()
                        })
                    } else if action == "open" {
                        if let Some(code) = self.capture_open_errors.get(&provider) {
                            error(code)
                        } else {
                            let mut result = json!({"ok":true,"stream":41});
                            if let Some(geometry) = &self.capture_geometry {
                                result["desktop_geometry"] = geometry.clone();
                            }
                            result
                        }
                    } else {
                        json!({"ok":true,"stream":41})
                    }
                } else {
                    let mut result = json!({"ok":true,"session_id":51,"packet_boundaries":payload["packet_boundaries"]});
                    if !self.omit_reference_metadata {
                        result["max_reference_frames"] =
                            payload["configuration"]["max_reference_frames"].clone();
                    }
                    result
                };
                self.pending.insert(self.next, result);
                if std::mem::take(&mut self.lose_accept) {
                    return Err("lost accepted reply".into());
                }
                Response::Accepted { id: self.next }
            }
            Command::TakeResult { request } => {
                if std::mem::take(&mut self.lose_result) {
                    return Err("lost result reply".into());
                }
                Response::Result {
                    payload: self
                        .pending
                        .remove(&request)
                        .unwrap_or(json!({"ok":false,"code":"not_found"})),
                }
            }
            Command::Cancel { .. } if self.cancel_failures > 0 => {
                self.cancel_failures -= 1;
                return Err("cancel lost".into());
            }
            Command::ReleaseResult { .. } if self.release_failures > 0 => {
                self.release_failures -= 1;
                return Err("release lost".into());
            }
            Command::Complete { request, payload } => {
                if self.parent_failures > 0 {
                    self.parent_failures -= 1;
                    return Err("parent completion lost".into());
                }
                self.ended.insert(request);
                self.completed.push((request, payload));
                if self.lost_complete {
                    Response::error("request_ended", "")
                } else {
                    Response::Ok
                }
            }
            _ => Response::Ok,
        })
    }
}

fn capture_provider_for_service(service: &str) -> Option<CaptureProvider> {
    [CaptureProvider::Dxgi, CaptureProvider::Wgc]
        .into_iter()
        .find(|provider| provider.service() == service)
}

#[derive(Default)]
struct FakePlane {
    calls: usize,
    drains: usize,
    inputs: Vec<(CaptureProvider, bool, bool, bool, bool, u64, u64)>,
    dropped: bool,
    fail: bool,
    freshness_api: bool,
    capture_timestamp_api: bool,
    capture_pacing_timestamp_us: u64,
    capture_observed_at: Option<Instant>,
    capture_reused: bool,
    geometry_checks: Vec<Option<CaptureGeometry>>,
    geometry_results: VecDeque<Result<GeometryProgress, &'static str>>,
}

impl Plane for FakePlane {
    fn verify_geometry(
        &mut self,
        _: CaptureProvider,
        _: u64,
        expected: Option<CaptureGeometry>,
    ) -> Result<GeometryProgress, &'static str> {
        self.geometry_checks.push(expected);
        self.geometry_results
            .pop_front()
            .unwrap_or(Ok(GeometryProgress::Ready))
    }

    fn pump(&mut self, input: Pump<'_>) -> Result<Progress, &'static str> {
        self.calls += 1;
        self.inputs.push((
            input.provider,
            input.capture_due,
            input.force_idr,
            input.repeat_due,
            input.repeat_frame.is_some(),
            input.sequence,
            input.timestamp_us,
        ));
        if self.fail {
            Err("frame_rejected")
        } else {
            let capture_reused = input.capture_due && input.constant_rate && self.capture_reused;
            let fresh_capture = input.capture_due && !capture_reused;
            let capture_submitted = input.capture_due && !self.dropped;
            let repeat_attempted = !capture_submitted
                && !self.dropped
                && input.repeat_due
                && input.repeat_frame.is_some();
            Ok(Progress {
                dropped: self.dropped,
                submitted: capture_submitted || repeat_attempted,
                repeat_attempted,
                capture_reused,
                capture_arrival: (fresh_capture
                    && input.provider == CaptureProvider::Wgc
                    && (!input.constant_rate || self.freshness_api || self.capture_timestamp_api))
                    .then(|| CaptureArrival {
                        timestamp_us: if self.capture_timestamp_api {
                            self.capture_pacing_timestamp_us
                        } else {
                            input.sequence.saturating_mul(16_667).max(1)
                        },
                        observed_at: self.capture_observed_at.unwrap_or_else(Instant::now),
                    }),
                frame: capture_submitted.then_some(FrameMetadata {
                    width: input.configuration.width,
                    height: input.configuration.height,
                    stride: input.configuration.width * 4,
                    timestamp_us: input.timestamp_us,
                    sequence: input.sequence,
                }),
                owned_frame: capture_submitted.then_some(OwnedFrame {
                    width: input.configuration.width,
                    height: input.configuration.height,
                    format: amaterasu_media::PIXEL_BGRA8,
                    stride: input.configuration.width * 4,
                    bytes: vec![
                        0;
                        input.configuration.width as usize
                            * input.configuration.height as usize
                            * 4
                    ],
                }),
            })
        }
    }
    fn drain(&mut self, _: u64) -> Result<bool, &'static str> {
        self.drains += 1;
        if self.fail {
            Err("encoder_drain_failed")
        } else {
            Ok(true)
        }
    }
}

fn geometry() -> CaptureGeometry {
    CaptureGeometry::new(
        DesktopRect::new(-1920, 0, 1920, 1080).unwrap(),
        DesktopRect::new(-1920, -200, 3840, 1440).unwrap(),
    )
    .unwrap()
}

fn encoder_identity() -> EncoderIdentity {
    EncoderIdentity {
        clsid: "{00000000-0000-0000-0000-000000000001}".into(),
        hardware_url: Some("test://encoder/nvidia".into()),
        friendly_name: "Test NVIDIA H.264".into(),
    }
}
fn setup() -> (Pipeline, Runtime, FakePlane) {
    setup_with_providers([CaptureProvider::Dxgi].into())
}

fn setup_with_providers(
    capture_providers: BTreeSet<CaptureProvider>,
) -> (Pipeline, Runtime, FakePlane) {
    (
        Pipeline::new(
            Settings {
                enabled: true,
                capture_providers,
                ..Settings::default()
            },
            "test".into(),
            Arc::new(Mutex::new(Queues::default())),
        ),
        Runtime::default(),
        FakePlane::default(),
    )
}

fn canonical_open(capture_mode: &str, provider: CaptureProvider, output: &str) -> Operation {
    serde_json::from_value(json!({
        "action":"open",
        "capture":capture_mode,
        "capture_provider":provider,
        "output":output,
        "encoder":{
            "clsid":"{00000000-0000-0000-0000-000000000001}",
            "friendly_name":"Test NVIDIA H.264",
            "hardware_url":"test://encoder/nvidia"
        },
        "configuration":{
            "width":1920,
            "height":1080,
            "fps":60,
            "bitrate":5000000
        }
    }))
    .unwrap()
}

fn capture_output(id: &str, device_name: &str) -> Value {
    json!({
        "id": id,
        "device_name": device_name,
        "attached": true,
        "left": 0,
        "top": 0,
        "width": 1920,
        "height": 1080,
        "rotation": 1,
        "adapter_vendor_id": 0x10de,
        "adapter_device_id": 0x1234
    })
}

fn open() -> Operation {
    Operation::Open {
        packet_boundaries: false,
        operation_id: None,
        capture_provider: CaptureProvider::Dxgi,
        auto_capture: false,
        constant_rate: false,
        wgc_pacing_smoothing: true,
        minimum_fps_target: 20.0,
        output: "display-1".into(),
        encoder: encoder_identity(),
        configuration: Configuration {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate: 5_000_000,
            max_reference_frames: 0,
            amd_av1_latency_mode: AmdAv1LatencyMode::Auto,
        },
    }
}

#[test]
fn required_reference_frame_limit_is_forwarded_and_confirmed() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    let mut operation = open();
    let Operation::Open { configuration, .. } = &mut operation else {
        unreachable!();
    };
    configuration.max_reference_frames = 1;
    pipeline.handle("alice", 99, operation, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    assert_eq!(pipeline.streams[&1].stage as u8, Stage::Running as u8);
    assert!(runtime.commands.iter().any(|command| matches!(
        command,
        Command::Request { service, payload, .. }
            if service == ENCODE
                && payload["configuration"]["max_reference_frames"] == 1
    )));
    assert_eq!(
        runtime.completed[0].1["configuration"]["max_reference_frames"],
        1
    );
}

#[test]
fn unsupported_amd_av1_latency_modes_fail_before_opening_lower_resources() {
    for mode in [
        AmdAv1LatencyMode::None,
        AmdAv1LatencyMode::PowerSaving,
        AmdAv1LatencyMode::Realtime,
        AmdAv1LatencyMode::Lowest,
    ] {
        let (mut pipeline, mut runtime, plane) = setup();
        let mut operation = open();
        let Operation::Open { configuration, .. } = &mut operation else {
            unreachable!();
        };
        configuration.amd_av1_latency_mode = mode;

        pipeline.handle("alice", 99, operation, &mut |command| runtime.send(command));

        assert_eq!(
            runtime.completed[0].1["code"],
            "amd_av1_latency_mode_unsupported"
        );
        assert!(!runtime.commands.iter().any(|command| matches!(
            command,
            Command::Request { service, .. } if service == CAPTURE || service == ENCODE
        )));
        assert_eq!(runtime.open_effects, 0);
        assert!(pipeline.streams.is_empty());
        assert_eq!(pipeline.next_id, 0);
        assert_eq!(plane.calls, 0);
    }
}

#[test]
fn constant_rate_delivery_is_opt_in_and_only_forwards_for_wgc() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    let mut request = open();
    let Operation::Open {
        capture_provider,
        constant_rate,
        ..
    } = &mut request
    else {
        unreachable!();
    };
    *capture_provider = CaptureProvider::Wgc;
    *constant_rate = true;
    pipeline.providers.insert(CaptureProvider::Wgc);

    pipeline.handle("alice", 99, request, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);

    assert_eq!(runtime.completed[0].1["constant_rate"], true);
    assert_eq!(runtime.completed[0].1["wgc_pacing_smoothing"], true);
    assert!(runtime.commands.iter().any(|command| matches!(
        command,
        Command::Request { service, payload, .. }
            if service == "amaterasu.capture-wgc/1"
                && payload["action"] == "open"
                && payload["constant_rate"] == true
                && payload.get("wgc_pacing_smoothing").is_none()
    )));

    let (mut pipeline, mut runtime, _) = setup();
    let mut request = open();
    let Operation::Open { constant_rate, .. } = &mut request else {
        unreachable!();
    };
    *constant_rate = true;
    pipeline.handle("alice", 100, request, &mut |command| runtime.send(command));
    assert_eq!(runtime.completed[0].1["code"], "constant_rate_requires_wgc");
    assert_eq!(runtime.open_effects, 0);
}

#[test]
fn wgc_pacing_tracks_capture_arrival_independently_of_encoder_backpressure() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    let mut request = open();
    let Operation::Open {
        capture_provider,
        configuration,
        ..
    } = &mut request
    else {
        unreachable!();
    };
    *capture_provider = CaptureProvider::Wgc;
    configuration.width = 16;
    configuration.height = 16;
    configuration.fps = 60;
    configuration.bitrate = 100_000;
    pipeline.providers.insert(CaptureProvider::Wgc);
    pipeline.handle("alice", 99, request, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 5);

    assert_eq!(pipeline.streams[&1].wgc_pacing.group_frames, 1);
    let submitted = pipeline.streams[&1].submitted;
    plane.dropped = true;
    pipeline.streams.get_mut(&1).unwrap().next_frame = Instant::now();
    tick(&mut pipeline, &mut runtime, &mut plane, 1);

    let stream = &pipeline.streams[&1];
    assert_eq!(stream.wgc_pacing.group_frames, 2);
    assert_eq!(stream.submitted, submitted);
    assert_eq!(stream.dropped, 1);
}

#[test]
fn constant_rate_cached_reuse_does_not_drive_wgc_pacing() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    let mut request = open();
    let Operation::Open {
        capture_provider,
        constant_rate,
        configuration,
        ..
    } = &mut request
    else {
        unreachable!();
    };
    *capture_provider = CaptureProvider::Wgc;
    *constant_rate = true;
    configuration.width = 16;
    configuration.height = 16;
    configuration.fps = 60;
    configuration.bitrate = 100_000;
    pipeline.providers.insert(CaptureProvider::Wgc);
    pipeline.handle("alice", 99, request, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 5);
    assert!(pipeline.streams[&1].last_pixels.is_some());

    pipeline.streams.get_mut(&1).unwrap().next_frame = Instant::now();
    tick(&mut pipeline, &mut runtime, &mut plane, 1);

    assert_eq!(pipeline.streams[&1].wgc_pacing.group_frames, 0);
}

#[test]
fn v1_2_constant_rate_reuses_advance_the_grid_without_reanchoring() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    let mut request = open();
    let Operation::Open {
        capture_provider,
        constant_rate,
        configuration,
        ..
    } = &mut request
    else {
        unreachable!();
    };
    *capture_provider = CaptureProvider::Wgc;
    *constant_rate = true;
    configuration.width = 16;
    configuration.height = 16;
    configuration.fps = 60;
    configuration.bitrate = 100_000;
    pipeline.providers.insert(CaptureProvider::Wgc);
    plane.freshness_api = true;
    pipeline.handle("alice", 99, request, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 5);

    assert_eq!(pipeline.streams[&1].wgc_pacing.group_frames, 1);
    let first_scheduled_slot = pipeline.streams[&1].next_frame;

    plane.capture_reused = true;
    pipeline.streams.get_mut(&1).unwrap().next_frame = Instant::now();
    tick(&mut pipeline, &mut runtime, &mut plane, 1);

    assert_eq!(pipeline.streams[&1].wgc_pacing.group_frames, 2);
    let repeated_scheduled_slot = pipeline.streams[&1].next_frame;
    assert_eq!(
        repeated_scheduled_slot.duration_since(first_scheduled_slot),
        Duration::from_nanos(16_666_667)
    );

    plane.capture_reused = false;
    pipeline.streams.get_mut(&1).unwrap().next_frame = Instant::now();
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    assert_eq!(pipeline.streams[&1].wgc_pacing.group_frames, 3);
    assert!(pipeline.streams[&1].next_frame > repeated_scheduled_slot);
}

#[test]
fn v1_3_constant_rate_capture_timestamp_reanchors_after_cached_reuse() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    let mut request = open();
    let Operation::Open {
        capture_provider,
        constant_rate,
        configuration,
        ..
    } = &mut request
    else {
        unreachable!();
    };
    *capture_provider = CaptureProvider::Wgc;
    *constant_rate = true;
    configuration.width = 16;
    configuration.height = 16;
    configuration.fps = 60;
    configuration.bitrate = 100_000;
    pipeline.providers.insert(CaptureProvider::Wgc);
    pipeline.handle("alice", 99, request, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);

    let opened_at = pipeline.streams[&1].opened_at;
    plane.capture_timestamp_api = true;
    plane.capture_pacing_timestamp_us = 100_000;
    plane.capture_observed_at = Some(opened_at + Duration::from_millis(10));
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    assert_eq!(pipeline.streams[&1].wgc_pacing.group_frames, 1);

    let missed_slot = pipeline.streams[&1].next_frame.duration_since(opened_at);
    pipeline
        .streams
        .get_mut(&1)
        .unwrap()
        .wgc_pacing
        .capture_missed(missed_slot);
    plane.capture_reused = true;
    pipeline.streams.get_mut(&1).unwrap().next_frame = Instant::now();
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    assert_eq!(pipeline.streams[&1].wgc_pacing.group_frames, 0);

    plane.capture_reused = false;
    plane.capture_pacing_timestamp_us = 133_500;
    plane.capture_observed_at = Some(opened_at + Duration::from_micros(51_500));
    pipeline.streams.get_mut(&1).unwrap().next_frame = Instant::now();
    tick(&mut pipeline, &mut runtime, &mut plane, 1);

    let stream = &pipeline.streams[&1];
    assert_eq!(stream.wgc_pacing.group_frames, 1);
    assert_eq!(
        stream.next_frame.duration_since(opened_at),
        Duration::from_nanos(59_999_998)
    );
}

#[test]
fn wgc_source_clock_jump_does_not_stall_running_capture() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    let mut request = open();
    let Operation::Open {
        capture_provider,
        configuration,
        ..
    } = &mut request
    else {
        unreachable!();
    };
    *capture_provider = CaptureProvider::Wgc;
    configuration.width = 16;
    configuration.height = 16;
    configuration.fps = 60;
    configuration.bitrate = 100_000;
    pipeline.providers.insert(CaptureProvider::Wgc);
    pipeline.handle("alice", 99, request, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);

    let opened_at = pipeline.streams[&1].opened_at;
    plane.capture_timestamp_api = true;
    plane.capture_pacing_timestamp_us = 100_000;
    plane.capture_observed_at = Some(opened_at + Duration::from_millis(10));
    tick(&mut pipeline, &mut runtime, &mut plane, 1);

    let missed_slot = pipeline.streams[&1].next_frame.duration_since(opened_at);
    pipeline
        .streams
        .get_mut(&1)
        .unwrap()
        .wgc_pacing
        .capture_missed(missed_slot);

    plane.capture_pacing_timestamp_us = 10_000_000;
    plane.capture_observed_at = Some(opened_at + Duration::from_millis(30));
    pipeline.streams.get_mut(&1).unwrap().next_frame = Instant::now();
    tick(&mut pipeline, &mut runtime, &mut plane, 1);

    let stream = &pipeline.streams[&1];
    assert!(matches!(stream.stage, Stage::Running));
    assert_eq!(stream.wgc_pacing.group_frames, 1);
    assert!(stream.next_frame < opened_at + Duration::from_millis(100));
}

#[test]
fn wgc_source_clock_ahead_of_callback_keeps_running_cadence() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    let mut request = open();
    let Operation::Open {
        capture_provider,
        configuration,
        ..
    } = &mut request
    else {
        unreachable!();
    };
    *capture_provider = CaptureProvider::Wgc;
    configuration.width = 16;
    configuration.height = 16;
    configuration.fps = 60;
    configuration.bitrate = 100_000;
    pipeline.providers.insert(CaptureProvider::Wgc);
    pipeline.handle("alice", 99, request, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);

    let opened_at = pipeline.streams[&1].opened_at;
    plane.capture_timestamp_api = true;
    plane.capture_pacing_timestamp_us = 100_000;
    plane.capture_observed_at = Some(opened_at + Duration::from_millis(10));
    tick(&mut pipeline, &mut runtime, &mut plane, 1);

    let missed_slot = pipeline.streams[&1].next_frame.duration_since(opened_at);
    pipeline
        .streams
        .get_mut(&1)
        .unwrap()
        .wgc_pacing
        .capture_missed(missed_slot);

    // The source clock jumps 150 ms while the next callback arrives at 30 ms.
    // A deadline derived from that future timestamp would pause this stream.
    plane.capture_pacing_timestamp_us = 250_000;
    plane.capture_observed_at = Some(opened_at + Duration::from_millis(30));
    pipeline.streams.get_mut(&1).unwrap().next_frame = Instant::now();
    tick(&mut pipeline, &mut runtime, &mut plane, 1);

    let stream = &pipeline.streams[&1];
    assert!(matches!(stream.stage, Stage::Running));
    assert_eq!(stream.wgc_pacing.group_frames, 1);
    assert!(stream.next_frame < opened_at + Duration::from_millis(70));
}

#[test]
fn legacy_capture_without_geometry_remains_supported_and_omits_metadata() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);

    assert_eq!(pipeline.streams[&1].stage as u8, Stage::Running as u8);
    assert_eq!(plane.geometry_checks, vec![None, None]);
    assert!(runtime.completed[0].1.get("desktop_geometry").is_none());
}

#[test]
fn malformed_capture_geometry_fails_before_encoder_open_and_closes_capture() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    runtime.capture_geometry = Some(json!({
        "output":{"left":0,"top":0,"width":0,"height":1080},
        "virtual_desktop":{"left":0,"top":0,"width":1920,"height":1080}
    }));
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 12);

    assert!(runtime.commands.iter().all(|command| !matches!(command,
        Command::Request { service, payload, .. }
            if service == ENCODE && payload["action"] == "open"
    )));
    assert!(runtime.commands.iter().any(|command| matches!(command,
        Command::Request { service, payload, .. }
            if service == CAPTURE && payload["action"] == "close" && payload["stream"] == 41
    )));
    assert_eq!(runtime.completed[0].1["code"], "invalid_capture_geometry");
}

#[test]
fn initial_native_geometry_mismatch_fails_before_encoder_activation() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    runtime.capture_geometry = Some(json!(geometry()));
    plane
        .geometry_results
        .push_back(Err("capture_geometry_changed"));
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 12);

    assert_eq!(plane.geometry_checks, vec![Some(geometry())]);
    assert_eq!(plane.calls, 0);
    assert!(runtime.commands.iter().all(|command| !matches!(command,
        Command::Request { service, payload, .. }
            if service == ENCODE && payload["action"] == "open"
    )));
    assert_eq!(runtime.completed[0].1["code"], "capture_geometry_changed");
}

#[test]
fn geometry_change_during_encoder_open_prevents_pipeline_activation() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    runtime.capture_geometry = Some(json!(geometry()));
    plane
        .geometry_results
        .push_back(Ok(GeometryProgress::Ready));
    plane
        .geometry_results
        .push_back(Err("capture_geometry_changed"));
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);

    assert_eq!(pipeline.streams[&1].stage as u8, Stage::FindEncoder as u8);
    assert_eq!(pipeline.streams[&1].session, Some(51));
    assert_eq!(plane.calls, 0);
    tick(&mut pipeline, &mut runtime, &mut plane, 12);
    assert!(runtime.commands.iter().any(|command| matches!(command,
        Command::Request { service, payload, .. }
            if service == ENCODE && payload["action"] == "close" && payload["session_id"] == 51
    )));
    assert!(runtime.commands.iter().any(|command| matches!(command,
        Command::Request { service, payload, .. }
            if service == CAPTURE && payload["action"] == "close" && payload["stream"] == 41
    )));
    assert_eq!(runtime.completed[0].1["code"], "capture_geometry_changed");
}

#[test]
fn busy_geometry_defers_activation_without_reopening_encoder() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    runtime.capture_geometry = Some(json!(geometry()));
    plane
        .geometry_results
        .push_back(Ok(GeometryProgress::Ready));
    plane.geometry_results.push_back(Ok(GeometryProgress::Busy));
    plane
        .geometry_results
        .push_back(Ok(GeometryProgress::Ready));
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);

    assert_eq!(pipeline.streams[&1].stage as u8, Stage::Activating as u8);
    assert!(runtime.completed.is_empty());
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    assert_eq!(pipeline.streams[&1].stage as u8, Stage::Running as u8);
    assert_eq!(runtime.completed[0].1["stream"], 1);
    assert_eq!(
        runtime
            .commands
            .iter()
            .filter(|command| matches!(command,
                Command::Request { service, payload, .. }
                    if service == ENCODE && payload["action"] == "open"
            ))
            .count(),
        1
    );
}

fn runtime_geometry_failure(code: &'static str) {
    let (mut pipeline, mut runtime, mut plane) = setup();
    runtime.capture_geometry = Some(json!(geometry()));
    plane
        .geometry_results
        .push_back(Ok(GeometryProgress::Ready));
    plane
        .geometry_results
        .push_back(Ok(GeometryProgress::Ready));
    plane.geometry_results.push_back(Err(code));
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    assert_eq!(pipeline.streams[&1].stage as u8, Stage::Running as u8);
    assert_eq!(
        runtime.completed[0].1["desktop_geometry"],
        json!(geometry())
    );

    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    assert_eq!(pipeline.streams[&1].stage as u8, Stage::FindEncoder as u8);
    assert_eq!(pipeline.streams[&1].error, Some(code));
    assert_eq!(plane.calls, 0, "no frame is read after geometry failure");
    unsafe extern "C" fn reject_packet(
        _: *mut std::ffi::c_void,
        _: *const amaterasu_media::EncodedPacket,
    ) -> amaterasu_module_sdk::abi::Status {
        amaterasu_module_sdk::abi::STATUS_INTERNAL_ERROR
    }
    let sink = amaterasu_media::PacketSink {
        context: std::ptr::null_mut(),
        write: reject_packet,
    };
    // SAFETY: the sink callback and its null-unused context live for this call.
    assert_eq!(
        unsafe { pipeline.queues.lock().unwrap().receive("alice", 1, sink) },
        amaterasu_media::MEDIA_CLOSED
    );
}

#[test]
fn runtime_geometry_change_stops_delivery_before_reading_another_frame() {
    runtime_geometry_failure("capture_geometry_changed");
}

#[test]
fn runtime_geometry_loss_stops_delivery_before_reading_another_frame() {
    runtime_geometry_failure("capture_closed");
}

#[test]
fn required_reference_frame_limit_rejects_unconfirmed_encoder() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    runtime.omit_reference_metadata = true;
    let mut operation = open();
    let Operation::Open { configuration, .. } = &mut operation else {
        unreachable!();
    };
    configuration.max_reference_frames = 1;
    pipeline.handle("alice", 99, operation, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 12);
    assert_eq!(
        runtime.completed[0].1["code"],
        "reference_frame_constraint_unavailable"
    );
    assert!(pipeline.streams.is_empty());
    assert_eq!(plane.calls, 0);
}

#[test]
fn changed_reference_frame_limit_conflicts_with_open_operation() {
    let (mut pipeline, mut runtime, _plane) = setup();
    let mut original = open();
    let Operation::Open { operation_id, .. } = &mut original else {
        unreachable!();
    };
    *operation_id = Some("same-open".into());
    pipeline.handle("alice", 99, original, &mut |command| runtime.send(command));

    let mut changed = open();
    let Operation::Open {
        operation_id,
        configuration,
        ..
    } = &mut changed
    else {
        unreachable!();
    };
    *operation_id = Some("same-open".into());
    configuration.max_reference_frames = 1;
    pipeline.handle("alice", 100, changed, &mut |command| runtime.send(command));
    assert_eq!(
        runtime.completed.last().unwrap().1["code"],
        "operation_conflict"
    );
}

fn tick(pipeline: &mut Pipeline, runtime: &mut Runtime, plane: &mut FakePlane, count: usize) {
    for _ in 0..count {
        pipeline.tick(plane, &mut |command| runtime.send(command));
    }
}

#[test]
fn open_delegates_with_parent_and_running_pumps_native_plane() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    assert_eq!(pipeline.streams[&1].stage as u8, Stage::Running as u8);
    assert_eq!(runtime.completed[0].1["stream"], 1);
    let encoder_open = runtime
        .commands
        .iter()
        .find_map(|command| match command {
            Command::Request {
                service, payload, ..
            } if service == ENCODE && payload["action"] == "open" => Some(payload),
            _ => None,
        })
        .unwrap();
    assert!(encoder_open.get("packet_boundaries").is_none());
    assert!(
        encoder_open["configuration"]
            .get("max_reference_frames")
            .is_none()
    );
    assert!(
        runtime
            .commands
            .iter()
            .filter_map(|command| if let Command::Request { parent, .. } = command {
                Some(parent)
            } else {
                None
            })
            .all(|parent| *parent == Some(99))
    );
    plane.dropped = true;
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    assert_eq!(pipeline.streams[&1].dropped, 1);
}

#[test]
fn cancelled_open_recovers_owned_ids_and_closes_without_parent() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    runtime.cancelled = true;
    tick(&mut pipeline, &mut runtime, &mut plane, 9);
    assert!(pipeline.streams.is_empty());
    assert!(
        runtime
            .commands
            .iter()
            .any(|command| matches!(command, Command::Cancel { .. }))
    );
    let closes: Vec<_> = runtime
        .commands
        .iter()
        .filter_map(|command| {
            if let Command::Request {
                payload, parent, ..
            } = command
            {
                if payload["action"] == "close" {
                    Some((payload, parent))
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect();
    assert_eq!(closes.len(), 2);
    assert!(closes.iter().all(|(_, parent)| parent.is_none()));
    assert_eq!(closes[0].0["session_id"], 51);
    assert_eq!(closes[1].0["stream"], 41);
    assert_eq!(plane.calls, 0);
}

#[test]
fn lost_completion_does_not_keep_pipeline_running() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    runtime.lost_complete = true;
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 12);
    assert!(pipeline.streams.is_empty());
    assert_eq!(plane.calls, 0);
}

#[test]
fn wrong_owner_cannot_close_or_observe_stream() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    pipeline.handle("bob", 100, Operation::Close { stream: 1 }, &mut |command| {
        runtime.send(command)
    });
    assert_eq!(runtime.completed.last().unwrap().1["code"], "not_found");
    pipeline.handle(
        "bob",
        101,
        Operation::Status { stream: None },
        &mut |command| runtime.send(command),
    );
    assert_eq!(runtime.completed.last().unwrap().1["streams"], json!([]));
    assert_eq!(pipeline.streams[&1].stage as u8, Stage::Running as u8);
}

#[test]
fn encoder_cleanup_failure_still_closes_capture_and_retains_failure() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    runtime.fail_encoder_cleanup = true;
    pipeline.handle(
        "alice",
        100,
        Operation::Close { stream: 1 },
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 20);
    assert_eq!(pipeline.streams[&1].stage as u8, Stage::Failed as u8);
    assert!(runtime.commands.iter().any(|command|matches!(command,Command::Request {service,payload,..} if service==CAPTURE && payload["action"]=="close")));
    assert_eq!(runtime.commands.iter().filter(|command|matches!(command,Command::Request {service,payload,..} if service==ENCODE && payload["action"]=="lookup")).count(),3);
}

#[test]
fn disabled_open_has_no_provider_calls() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.enabled = false;
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 3);
    assert_eq!(runtime.completed[0].1["code"], "disabled");
    assert!(
        !runtime
            .commands
            .iter()
            .any(|command| matches!(command, Command::Request { .. }))
    );
}

#[test]
fn lost_lower_accept_or_result_reconciles_without_repeating_open() {
    for accepted in [true, false] {
        let (mut pipeline, mut runtime, mut plane) = setup();
        runtime.lose_accept = accepted;
        runtime.lose_result = !accepted;
        pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
        tick(&mut pipeline, &mut runtime, &mut plane, 12);
        assert!(pipeline.streams.is_empty());
        assert_eq!(plane.calls, 0);
        assert_eq!(runtime.open_effects, 1);
        assert!(runtime.commands.iter().any(|command| matches!(command,
            Command::Request {service,payload,parent:None,..} if service==CAPTURE && payload["action"]=="lookup")));
        assert!(runtime.commands.iter().any(|command| matches!(command,
            Command::Request {service,payload,parent:None,..} if service==CAPTURE && payload["stream"]==41 && payload["action"]=="close")));
    }
}

#[test]
fn repeated_close_does_not_cancel_or_restart_cleanup() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    pipeline.handle(
        "alice",
        100,
        Operation::Close { stream: 1 },
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    let pending = pipeline.streams[&1].pending;
    pipeline.handle(
        "alice",
        101,
        Operation::Close { stream: 1 },
        &mut |command| runtime.send(command),
    );
    assert_eq!(pipeline.streams[&1].pending, pending);
    assert!(
        !runtime
            .commands
            .iter()
            .any(|command| matches!(command, Command::Cancel { .. }))
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 8);
    assert!(pipeline.streams.is_empty());
}

#[test]
fn shutdown_invalidates_delivery_and_enqueues_exact_known_closes() {
    use amaterasu_media::{EncodedPacket, MEDIA_CLOSED, PacketSink};
    use amaterasu_module_sdk::abi::{STATUS_OK, Status};
    unsafe extern "C" fn unused(_: *mut std::ffi::c_void, _: *const EncodedPacket) -> Status {
        STATUS_OK
    }
    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    pipeline.shutdown(&mut |command| runtime.send(command));
    assert!(!pipeline.enabled);
    assert!(pipeline.streams.is_empty());
    let sink = PacketSink {
        context: std::ptr::null_mut(),
        write: unused,
    };
    assert_eq!(
        unsafe { pipeline.queues.lock().unwrap().receive("alice", 1, sink) },
        MEDIA_CLOSED
    );
    let closes: Vec<_> = runtime
        .commands
        .iter()
        .filter_map(|command| match command {
            Command::Request {
                service,
                payload,
                parent: None,
                ..
            } if payload["action"] == "close" => Some((service, payload)),
            _ => None,
        })
        .collect();
    assert_eq!(closes.len(), 2);
    assert_eq!(closes[0].0, ENCODE);
    assert_eq!(closes[0].1["session_id"], 51);
    assert_eq!(closes[1].0, CAPTURE);
    assert_eq!(closes[1].1["stream"], 41);
}

#[test]
fn shutdown_cancels_pending_open_and_ends_parent_without_replaying_it() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    pipeline.shutdown(&mut |command| runtime.send(command));
    assert!(
        runtime
            .commands
            .iter()
            .any(|command| matches!(command, Command::Cancel { request: 1 }))
    );
    assert_eq!(
        runtime.completed.last().unwrap().1["code"],
        "video_shutdown"
    );
    assert_eq!(
        runtime
            .commands
            .iter()
            .filter(|command| matches!(command, Command::Request { .. }))
            .count(),
        1
    );
}

#[test]
fn cancellation_or_release_failure_blocks_lookup_and_retains_retry_handle() {
    for cancel_failure in [true, false] {
        let (mut pipeline, mut runtime, mut plane) = setup();
        pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
        tick(&mut pipeline, &mut runtime, &mut plane, 1);
        runtime.cancelled = true;
        if cancel_failure {
            runtime.cancel_failures = 3;
        } else {
            runtime.release_failures = 3;
        }
        tick(&mut pipeline, &mut runtime, &mut plane, 4);
        assert_eq!(pipeline.streams[&1].stage as u8, Stage::Failed as u8);
        assert_eq!(pipeline.streams[&1].unsettled, Some(1));
        assert!(!runtime.commands.iter().any(
            |command| matches!(command,Command::Request {payload,..} if payload["action"]=="lookup")
        ));
        pipeline.handle(
            "alice",
            100,
            Operation::Close { stream: 1 },
            &mut |command| runtime.send(command),
        );
        tick(&mut pipeline, &mut runtime, &mut plane, 8);
        assert!(pipeline.streams.is_empty());
        assert_eq!(runtime.open_effects, 1);
    }
}

#[test]
fn unknown_accept_waits_for_confirmed_parent_end_before_broker_retry() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    runtime.lose_accept = true;
    runtime.parent_failures = 3;
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 5);
    assert_eq!(pipeline.streams[&1].stage as u8, Stage::Failed as u8);
    assert!(pipeline.streams[&1].uncertain.is_some());
    assert_eq!(
        runtime
            .commands
            .iter()
            .filter(|command| matches!(command, Command::Request { .. }))
            .count(),
        1
    );
    pipeline.handle(
        "alice",
        100,
        Operation::Close { stream: 1 },
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 8);
    assert!(pipeline.streams.is_empty());
    assert_eq!(runtime.open_effects, 1);
}

#[test]
fn expired_dedup_cannot_replay_open_after_parent_termination() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    runtime.lose_accept = true;
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    runtime.operations.clear(); // Model broker's retention expiry after the lost reply.
    tick(&mut pipeline, &mut runtime, &mut plane, 8);
    assert!(runtime.ended.contains(&99));
    assert_eq!(runtime.open_effects, 1);
    assert!(pipeline.streams.is_empty());
}

#[test]
fn canonical_auto_falls_back_to_the_same_output_with_fresh_operation_ids() {
    let dxgi_id = format!("00000000:00000001/{}", r"\\.\DISPLAY1");
    let wgc_id = r"\\.\DISPLAY1";
    let providers = [CaptureProvider::Dxgi, CaptureProvider::Wgc].into();
    let (mut pipeline, mut runtime, mut plane) = setup_with_providers(providers);
    runtime
        .capture_open_errors
        .insert(CaptureProvider::Dxgi, "unsupported_platform");
    runtime
        .capture_outputs
        .insert(CaptureProvider::Wgc, vec![capture_output(wgc_id, wgc_id)]);

    pipeline.handle(
        "alice",
        99,
        canonical_open("auto", CaptureProvider::Dxgi, &dxgi_id),
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 12);

    assert!(matches!(pipeline.streams[&1].stage, Stage::Running));
    assert_eq!(pipeline.streams[&1].provider, CaptureProvider::Wgc);
    assert_eq!(runtime.completed[0].1["output"], dxgi_id);
    assert_eq!(runtime.completed[0].1["capture_provider"], "wgc");
    let capture_opens: Vec<_> = runtime
        .commands
        .iter()
        .filter_map(|command| match command {
            Command::Request {
                service,
                payload,
                operation_id,
                ..
            } if capture_provider_for_service(service).is_some() && payload["action"] == "open" => {
                Some((service, payload, operation_id))
            }
            _ => None,
        })
        .collect();
    assert_eq!(capture_opens.len(), 2);
    assert_eq!(capture_opens[1].1["output"], wgc_id);
    assert_ne!(
        capture_opens[0].1["operation_id"],
        capture_opens[1].1["operation_id"]
    );
    assert_ne!(capture_opens[0].2, capture_opens[1].2);
    assert_eq!(
        runtime
            .commands
            .iter()
            .filter(|command| matches!(command, Command::Request { service, payload, .. } if service == CaptureProvider::Wgc.service() && payload["action"] == "enumerate"))
            .count(),
        1
    );
}

#[test]
fn canonical_auto_maps_wgc_device_name_to_unique_dxgi_identity() {
    let wgc_id = r"\\.\DISPLAY1";
    let dxgi_id = format!("00000000:00000001/{wgc_id}");
    let providers = [CaptureProvider::Dxgi, CaptureProvider::Wgc].into();
    let (mut pipeline, mut runtime, mut plane) = setup_with_providers(providers);
    runtime
        .capture_open_errors
        .insert(CaptureProvider::Wgc, "unsupported_platform");
    runtime.capture_outputs.insert(
        CaptureProvider::Dxgi,
        vec![capture_output(&dxgi_id, wgc_id)],
    );

    pipeline.handle(
        "alice",
        99,
        canonical_open("", CaptureProvider::Wgc, wgc_id),
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 12);

    assert!(matches!(pipeline.streams[&1].stage, Stage::Running));
    assert_eq!(pipeline.streams[&1].provider, CaptureProvider::Dxgi);
    assert_eq!(runtime.completed[0].1["output"], wgc_id);
    assert_eq!(runtime.completed[0].1["capture_provider"], "dxgi");
    assert!(runtime.commands.iter().any(|command| matches!(
        command,
        Command::Request { service, payload, .. }
            if service == CaptureProvider::Dxgi.service()
                && payload["action"] == "open"
                && payload["output"] == dxgi_id
    )));
}

#[test]
fn auto_retry_requires_clean_provider_rejection_and_allowlisted_candidate() {
    let providers: BTreeSet<CaptureProvider> = [CaptureProvider::Dxgi, CaptureProvider::Wgc].into();
    for code in [
        "permission_denied",
        "resource_exhausted",
        "invalid_request",
        "backend_error",
    ] {
        let (mut pipeline, mut runtime, mut plane) = setup_with_providers(providers.clone());
        runtime
            .capture_open_errors
            .insert(CaptureProvider::Dxgi, code);
        pipeline.handle(
            "alice",
            99,
            canonical_open("auto", CaptureProvider::Dxgi, "LUID/display-1"),
            &mut |command| runtime.send(command),
        );
        tick(&mut pipeline, &mut runtime, &mut plane, 12);
        assert_eq!(runtime.open_effects, 1, "error {code}");
        assert!(
            !runtime.commands.iter().any(|command| matches!(
                command,
                Command::Request { service, payload, .. }
                    if service == CaptureProvider::Wgc.service()
                        && matches!(payload["action"].as_str(), Some("open" | "enumerate"))
            )),
            "error {code}"
        );
    }

    let (mut pipeline, mut runtime, mut plane) = setup();
    runtime
        .capture_open_errors
        .insert(CaptureProvider::Dxgi, "unsupported_platform");
    pipeline.handle(
        "alice",
        99,
        canonical_open("auto", CaptureProvider::Dxgi, "LUID/display-1"),
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 8);
    assert_eq!(runtime.open_effects, 1);
    assert!(!runtime.commands.iter().any(|command| matches!(
        command,
        Command::Request { service, .. } if service == CaptureProvider::Wgc.service()
    )));

    let (mut pipeline, mut runtime, mut plane) = setup_with_providers(providers);
    runtime
        .capture_admission_errors
        .insert(CaptureProvider::Dxgi, "permission_denied");
    pipeline.handle(
        "alice",
        99,
        canonical_open("auto", CaptureProvider::Dxgi, "LUID/display-1"),
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 8);
    assert_eq!(runtime.open_effects, 0);
    assert!(!runtime.commands.iter().any(|command| matches!(
        command,
        Command::Request { service, .. } if service == CaptureProvider::Wgc.service()
    )));
}

#[test]
fn clean_unavailable_admission_retries_but_uncertain_admission_does_not() {
    let dxgi_id = format!("00000000:00000001/{}", r"\\.\DISPLAY1");
    let wgc_id = r"\\.\DISPLAY1";
    let providers: BTreeSet<CaptureProvider> = [CaptureProvider::Dxgi, CaptureProvider::Wgc].into();
    let (mut pipeline, mut runtime, mut plane) = setup_with_providers(providers.clone());
    runtime
        .capture_admission_errors
        .insert(CaptureProvider::Dxgi, "unavailable");
    runtime
        .capture_outputs
        .insert(CaptureProvider::Wgc, vec![capture_output(wgc_id, wgc_id)]);
    pipeline.handle(
        "alice",
        99,
        canonical_open("auto", CaptureProvider::Dxgi, &dxgi_id),
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 12);
    assert_eq!(pipeline.streams[&1].provider, CaptureProvider::Wgc);
    assert_ne!(
        pipeline.streams[&1].capture_operation,
        pipeline.streams[&1].operation
    );

    let (mut pipeline, mut runtime, mut plane) = setup_with_providers(providers);
    runtime.lose_accept = true;
    pipeline.handle(
        "alice",
        99,
        canonical_open("auto", CaptureProvider::Dxgi, &dxgi_id),
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 12);
    assert_eq!(runtime.open_effects, 1);
    assert!(!runtime.commands.iter().any(|command| matches!(
        command,
        Command::Request { payload, .. } if payload["action"] == "enumerate"
    )));
}

#[test]
fn ambiguous_candidate_output_keeps_the_original_provider_error() {
    let dxgi_id = format!("00000000:00000001/{}", r"\\.\DISPLAY1");
    let wgc_id = r"\\.\DISPLAY1";
    let providers = [CaptureProvider::Dxgi, CaptureProvider::Wgc].into();
    let (mut pipeline, mut runtime, mut plane) = setup_with_providers(providers);
    runtime
        .capture_open_errors
        .insert(CaptureProvider::Dxgi, "unsupported_platform");
    runtime.capture_outputs.insert(
        CaptureProvider::Wgc,
        vec![
            capture_output(wgc_id, wgc_id),
            capture_output(wgc_id, wgc_id),
        ],
    );

    pipeline.handle(
        "alice",
        99,
        canonical_open("auto", CaptureProvider::Dxgi, &dxgi_id),
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 12);

    assert!(pipeline.streams.is_empty());
    assert_eq!(pipeline.recent[0].1["error"], "unsupported_platform");
    assert_eq!(runtime.open_effects, 1);
    assert!(!runtime.commands.iter().any(|command| matches!(
        command,
        Command::Request { service, payload, .. }
            if service == CaptureProvider::Wgc.service() && payload["action"] == "open"
    )));
}

#[test]
fn explicit_capture_mode_remains_pinned_after_unsupported_provider_reply() {
    for (mode, bootstrap, expected_provider, constant_rate) in [
        ("ddx", CaptureProvider::Wgc, CaptureProvider::Dxgi, false),
        ("wgc", CaptureProvider::Dxgi, CaptureProvider::Wgc, false),
        ("wgcc", CaptureProvider::Dxgi, CaptureProvider::Wgc, true),
    ] {
        let providers = [CaptureProvider::Dxgi, CaptureProvider::Wgc].into();
        let (mut pipeline, mut runtime, mut plane) = setup_with_providers(providers);
        runtime
            .capture_open_errors
            .insert(expected_provider, "unsupported_platform");
        pipeline.handle(
            "alice",
            99,
            canonical_open(mode, bootstrap, "provider/output"),
            &mut |command| runtime.send(command),
        );
        tick(&mut pipeline, &mut runtime, &mut plane, 8);

        assert_eq!(runtime.open_effects, 1, "mode {mode}");
        assert!(
            !runtime.commands.iter().any(|command| matches!(
                command,
                Command::Request { payload, .. } if payload["action"] == "enumerate"
            )),
            "mode {mode}"
        );
        assert!(
            runtime.commands.iter().any(|command| matches!(
                command,
                Command::Request { service, payload, .. }
                    if service == expected_provider.service()
                        && payload["action"] == "open"
                        && (payload.get("constant_rate").and_then(Value::as_bool) == Some(true))
                            == constant_rate
            )),
            "mode {mode}"
        );
    }
}

#[test]
fn provider_allowlist_controls_native_selection_and_every_cleanup_call() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    let wgc = || match open() {
        Operation::Open {
            output,
            encoder,
            configuration,
            constant_rate,
            wgc_pacing_smoothing,
            minimum_fps_target,
            ..
        } => Operation::Open {
            packet_boundaries: false,
            operation_id: None,
            capture_provider: CaptureProvider::Wgc,
            auto_capture: false,
            constant_rate,
            wgc_pacing_smoothing,
            minimum_fps_target,
            output,
            encoder,
            configuration,
        },
        _ => unreachable!(),
    };
    pipeline.handle("alice", 99, wgc(), &mut |command| runtime.send(command));
    assert_eq!(
        runtime.completed.last().unwrap().1["code"],
        "capture_provider_not_allowed"
    );
    assert_eq!(runtime.open_effects, 0);
    pipeline.providers.insert(CaptureProvider::Wgc);
    pipeline.handle("alice", 100, wgc(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 5);
    assert_eq!(plane.inputs.last().unwrap().0, CaptureProvider::Wgc);
    pipeline.handle(
        "alice",
        101,
        Operation::Close { stream: 1 },
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 8);
    assert!(pipeline.streams.is_empty());
    assert!(
        !runtime
            .commands
            .iter()
            .any(|command| matches!(command,Command::Request {service,..} if service==CAPTURE))
    );
    assert!(runtime.commands.iter().any(|command|matches!(command,Command::Request {service,payload,..} if service==CaptureProvider::Wgc.service() && payload["action"]=="close")));
}

#[test]
fn force_idr_is_owned_and_survives_encoder_backpressure_until_acceptance() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    pipeline.handle(
        "bob",
        100,
        Operation::ForceIdr { stream: 1 },
        &mut |command| runtime.send(command),
    );
    assert_eq!(runtime.completed.last().unwrap().1["code"], "not_found");
    assert!(!pipeline.streams[&1].force_idr);
    pipeline.handle(
        "alice",
        101,
        Operation::ForceIdr { stream: 1 },
        &mut |command| runtime.send(command),
    );
    plane.dropped = true;
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    assert!(pipeline.streams[&1].force_idr);
    assert!(plane.inputs.last().unwrap().2);
    plane.dropped = false;
    pipeline.streams.get_mut(&1).unwrap().next_frame = Instant::now();
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    assert!(!pipeline.streams[&1].force_idr);
    assert!(plane.inputs.last().unwrap().2);
}

#[test]
fn minimum_fps_repeats_above_capture_rate_on_independent_ticks() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    let mut request = open();
    let Operation::Open {
        minimum_fps_target,
        configuration,
        ..
    } = &mut request
    else {
        unreachable!();
    };
    *minimum_fps_target = 120.0;
    configuration.width = 16;
    configuration.height = 16;
    configuration.fps = 30;
    configuration.bitrate = 100_000;
    pipeline.handle("alice", 99, request, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    tick(&mut pipeline, &mut runtime, &mut plane, 1);

    let now = Instant::now();
    let stream = pipeline.streams.get_mut(&1).unwrap();
    assert!(stream.last_pixels.is_some());
    assert_eq!(stream.configuration.fps, 30);
    stream.last_output_at = Some(now - Duration::from_millis(15));
    stream.last_repeat_attempt_at = None;
    stream.next_frame = now + Duration::from_secs(30);
    let sequence_before_repeat = stream.last_output_sequence;
    assert!(stream.repeat_due(now));

    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    let input = plane.inputs.last().unwrap();
    assert!(!input.1, "capture polling stays at the negotiated 30 FPS");
    assert!(input.3, "the independent 120 FPS idle target is due");
    assert!(input.4, "repeats use the owned accepted frame");
    assert_eq!(
        pipeline.streams[&1].last_output_sequence,
        sequence_before_repeat + 1
    );

    let stream = &pipeline.streams[&1];
    let accepted_at = stream.last_output_at.unwrap();
    assert!(!stream.repeat_due(accepted_at + Duration::from_millis(8)));
    assert!(stream.repeat_due(accepted_at + Duration::from_millis(9)));

    let stream = pipeline.streams.get_mut(&1).unwrap();
    stream.minimum_fps_target = 0.0;
    assert!(!stream.repeat_due(accepted_at + Duration::from_millis(33)));
    assert!(stream.repeat_due(accepted_at + Duration::from_millis(34)));
}

#[test]
fn accepted_fresh_capture_suppresses_a_due_repeat_on_the_same_tick() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    let mut request = open();
    let Operation::Open {
        capture_provider,
        constant_rate,
        minimum_fps_target,
        configuration,
        ..
    } = &mut request
    else {
        unreachable!();
    };
    *capture_provider = CaptureProvider::Wgc;
    *constant_rate = true;
    pipeline.providers.insert(CaptureProvider::Wgc);
    *minimum_fps_target = 120.0;
    configuration.width = 16;
    configuration.height = 16;
    configuration.fps = 30;
    configuration.bitrate = 100_000;
    pipeline.handle("alice", 99, request, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 5);

    let stream = pipeline.streams.get_mut(&1).unwrap();
    let previous_sequence = stream.last_output_sequence;
    let previous_submitted = stream.submitted;
    stream.last_output_at = Some(Instant::now() - Duration::from_millis(15));
    stream.next_frame = Instant::now();
    assert!(stream.repeat_due(Instant::now()));

    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    let input = plane.inputs.last().unwrap();
    assert!(input.1, "capture is due");
    assert!(input.3, "the idle target is also due");
    let stream = &pipeline.streams[&1];
    assert_eq!(stream.last_output_sequence, previous_sequence + 1);
    assert_eq!(stream.submitted, previous_submitted + 1);
    assert!(stream.last_repeat_attempt_at.is_none());
}

#[test]
fn capture_error_discards_the_repeat_cache_before_cleanup() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    let mut request = open();
    let Operation::Open { configuration, .. } = &mut request else {
        unreachable!();
    };
    configuration.width = 16;
    configuration.height = 16;
    configuration.fps = 30;
    configuration.bitrate = 100_000;
    pipeline.handle("alice", 99, request, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 5);
    assert!(pipeline.streams[&1].last_pixels.is_some());

    plane.fail = true;
    pipeline.streams.get_mut(&1).unwrap().next_frame = Instant::now();
    tick(&mut pipeline, &mut runtime, &mut plane, 1);

    assert_eq!(pipeline.streams[&1].stage as u8, Stage::FindEncoder as u8);
    assert!(pipeline.streams[&1].last_pixels.is_none());
    assert!(pipeline.streams[&1].last_output_at.is_none());
}

#[test]
fn drain_stops_capture_preserves_rejected_eos_and_cleans_after_acceptance() {
    use amaterasu_media::{CODEC_H264, EncodedPacket, FLAG_END_OF_STREAM, MEDIA_BUSY, PacketSink};
    use amaterasu_module_sdk::abi::{STATUS_OK, Status};
    unsafe extern "C" fn reject(_: *mut std::ffi::c_void, _: *const EncodedPacket) -> Status {
        MEDIA_BUSY
    }
    unsafe extern "C" fn accept(_: *mut std::ffi::c_void, packet: *const EncodedPacket) -> Status {
        assert_eq!(unsafe { (*packet).flags }, FLAG_END_OF_STREAM);
        STATUS_OK
    }
    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    pipeline.handle("bob", 100, Operation::Drain { stream: 1 }, &mut |command| {
        runtime.send(command)
    });
    assert_eq!(runtime.completed.last().unwrap().1["code"], "not_found");
    pipeline.handle(
        "alice",
        101,
        Operation::Drain { stream: 1 },
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 3);
    assert_eq!(plane.drains, 1);
    assert!(
        plane
            .inputs
            .iter()
            .all(|(_, capture, force, ..)| !*capture && !*force)
    );
    assert!(pipeline.streams[&1].capture.is_none());
    pipeline.handle(
        "alice",
        102,
        Operation::Drain { stream: 1 },
        &mut |command| runtime.send(command),
    );
    let eos = EncodedPacket {
        codec: CODEC_H264,
        flags: FLAG_END_OF_STREAM,
        data: std::ptr::null(),
        len: 0,
        timestamp_us: 0,
        sequence: 0,
    };
    assert_eq!(
        unsafe { pipeline.queues.lock().unwrap().push(1, &eos) },
        STATUS_OK
    );
    assert_eq!(
        unsafe {
            pipeline.queues.lock().unwrap().receive(
                "alice",
                1,
                PacketSink {
                    context: std::ptr::null_mut(),
                    write: reject,
                },
            )
        },
        MEDIA_BUSY
    );
    let before_wait = plane.calls;
    tick(&mut pipeline, &mut runtime, &mut plane, 2);
    assert_eq!(plane.calls, before_wait);
    assert_eq!(pipeline.streams[&1].stage as u8, Stage::Draining as u8);
    assert_eq!(plane.drains, 1);
    assert_eq!(
        unsafe {
            pipeline.queues.lock().unwrap().receive(
                "alice",
                1,
                PacketSink {
                    context: std::ptr::null_mut(),
                    write: accept,
                },
            )
        },
        STATUS_OK
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 8);
    assert!(pipeline.streams.is_empty());
    assert!(pipeline.recent.is_empty());
}

#[test]
fn drain_timeout_settles_pending_capture_close_and_reports_failure_after_cleanup() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    pipeline.handle(
        "alice",
        100,
        Operation::Drain { stream: 1 },
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    output::expire(pipeline.streams.get_mut(&1).unwrap());
    runtime.cancel_failures = 3;
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    assert_eq!(plane.drains, 0);
    assert_eq!(pipeline.streams[&1].stage as u8, Stage::Failed as u8);
    assert!(pipeline.streams[&1].unsettled.is_some());
    assert!(!runtime.commands.iter().any(
        |command| matches!(command,Command::Request {payload,..} if payload["action"]=="lookup")
    ));

    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    pipeline.handle(
        "alice",
        100,
        Operation::Drain { stream: 1 },
        &mut |command| runtime.send(command),
    );
    output::expire(pipeline.streams.get_mut(&1).unwrap());
    tick(&mut pipeline, &mut runtime, &mut plane, 8);
    assert!(pipeline.streams.is_empty());
    assert_eq!(pipeline.recent[0].1["error"], "drain_timeout");
    pipeline.handle(
        "bob",
        101,
        Operation::Status { stream: None },
        &mut |command| runtime.send(command),
    );
    assert_eq!(runtime.completed.last().unwrap().1["streams"], json!([]));
}

#[test]
fn native_drain_error_is_visible_after_resource_cleanup() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    pipeline.handle(
        "alice",
        100,
        Operation::Drain { stream: 1 },
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 2);
    plane.fail = true;
    tick(&mut pipeline, &mut runtime, &mut plane, 8);
    assert!(pipeline.streams.is_empty());
    assert_eq!(pipeline.recent[0].1["error"], "encoder_drain_failed");
    assert_eq!(plane.drains, 1);
}

mod operations;
