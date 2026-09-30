use super::*;
use crate::model::AmdAv1LatencyMode;
use crate::plane::{CaptureArrival, FrameMetadata, GeometryProgress, OwnedFrame, Progress};
use amaterasu_media::{CaptureGeometry, DesktopRect};
use amaterasu_module_sdk::EncoderIdentity;
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
    capture_touch_surface: Option<Value>,
    capture_open_errors: BTreeMap<CaptureProvider, &'static str>,
    capture_admission_errors: BTreeMap<CaptureProvider, &'static str>,
    capture_outputs: BTreeMap<CaptureProvider, Vec<Value>>,
    capture_page_size: Option<usize>,
    capture_outputs_after_first_page: Option<Vec<Value>>,
    capture_enumeration_override: Option<Value>,
    encoder_enumeration: Option<Value>,
    hold_encoder_enumeration: bool,
    encoder_probe_replies: BTreeMap<String, Value>,
    hold_encoder_probes: bool,
    lose_probe_accept: bool,
    encoder_open_error: Option<&'static str>,
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
                } else if service == ENCODE && action == "enumerate" {
                    self.encoder_enumeration.clone().unwrap_or_else(|| {
                        json!({
                            "ok":true,"enabled":true,"encoders":[{
                                "identity":encoder_identity(),"name":"Test NVIDIA H.264",
                                "codec":"h264","hardware":true,"vendor_id":0x10de
                            }]
                        })
                    })
                } else if service == ENCODE && action == "probe" {
                    self.encoder_probe_replies
                        .get(payload["encoder"]["clsid"].as_str().unwrap())
                        .cloned()
                        .unwrap_or_else(|| {
                            probe_reply(&payload["encoder"], &payload["configuration"], true)
                        })
                } else if service == ENCODE && action == "open" && self.encoder_open_error.is_some()
                {
                    error(self.encoder_open_error.unwrap())
                } else if let Some(provider) = capture_provider {
                    if action == "enumerate" {
                        if let Some(reply) = &self.capture_enumeration_override {
                            reply.clone()
                        } else {
                            let changed_page = payload.get("offset").is_some()
                                && self.capture_outputs_after_first_page.is_some();
                            let outputs = if changed_page {
                                self.capture_outputs_after_first_page.clone().unwrap()
                            } else {
                                self.capture_outputs
                                    .get(&provider)
                                    .cloned()
                                    .unwrap_or_default()
                            };
                            if let Some(page_size) = self.capture_page_size {
                                let offset =
                                    payload.get("offset").and_then(Value::as_u64).unwrap_or(0)
                                        as usize;
                                let end = offset.saturating_add(page_size).min(outputs.len());
                                let mut reply = json!({"ok":true,"outputs":outputs[offset..end]});
                                reply["enumeration_id"] = json!(if changed_page {
                                    "b".repeat(64)
                                } else {
                                    "a".repeat(64)
                                });
                                if end < outputs.len() {
                                    reply["next_offset"] = json!(end);
                                }
                                reply
                            } else {
                                json!({"ok":true,"outputs":outputs})
                            }
                        }
                    } else if action == "open" {
                        if let Some(code) = self.capture_open_errors.get(&provider) {
                            error(code)
                        } else {
                            let mut result = json!({"ok":true,"stream":41});
                            if let Some(geometry) = &self.capture_geometry {
                                result["desktop_geometry"] = geometry.clone();
                            }
                            if let Some(touch_surface) = &self.capture_touch_surface {
                                result["touch_surface"] = touch_surface.clone();
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
                if std::mem::take(&mut self.lose_accept)
                    || action == "probe" && std::mem::take(&mut self.lose_probe_accept)
                {
                    return Err("lost accepted reply".into());
                }
                Response::Accepted { id: self.next }
            }
            Command::TakeResult { request } => {
                if self.hold_encoder_probes
                    && self.commands.iter().any(|command| {
                        matches!(command, Command::Request { service, operation_id, payload, .. }
                        if service == ENCODE && payload["action"] == "probe"
                        && self.operations.get(operation_id) == Some(&request))
                    })
                {
                    return Ok(Response::Pending);
                }
                if self.hold_encoder_enumeration
                    && self.commands.iter().any(|command| {
                        matches!(command, Command::Request { service, operation_id, payload, .. }
                        if service == ENCODE && payload["action"] == "enumerate"
                        && self.operations.get(operation_id) == Some(&request))
                    })
                {
                    return Ok(Response::Pending);
                }
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
    [
        CaptureProvider::Dxgi,
        CaptureProvider::Wgc,
        CaptureProvider::PipeWire,
    ]
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
    failure_code: Option<&'static str>,
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
        if let Some(code) = self.failure_code {
            Err(code)
        } else if self.fail {
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

fn probe_reply(identity: &Value, configuration: &Value, usable: bool) -> Value {
    json!({"ok":true,"codec":"h264","encoder":identity,"configuration":configuration,
        "usable":usable,"settled":true,
        "code":if usable { None } else { Some("encoder_unusable") },
        "reason":if usable { None } else { Some("encoder_open_failed") }})
}

fn auto_open() -> Operation {
    let mut operation = open();
    let Operation::Open { encoder, .. } = &mut operation else {
        unreachable!()
    };
    *encoder = EncoderSelection::Backend(EncoderBackend::Auto);
    operation
}

fn candidate_description(clsid: &str, hardware: bool) -> Value {
    json!({"identity":{"clsid":clsid,"hardware_url":null,"friendly_name":"H.264"},
        "name":"H.264","codec":"h264","hardware":hardware,"vendor_id":null})
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

fn pipewire_output(
    id: &str,
    width: u32,
    height: u32,
    source_type: &str,
    geometry_supported: bool,
) -> Value {
    json!({
        "id": id,
        "source_type": source_type,
        "width": width,
        "height": height,
        "format": "bgra8",
        "pipewire_node": 42,
        "geometry_supported": geometry_supported
    })
}

fn pipewire_enumeration(output: Value) -> Value {
    json!({
        "ok": true,
        "outputs": [output.clone()],
        "enumeration_id": output["id"],
        "geometry_supported": false
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
        encoder: amaterasu_module_sdk::EncoderSelection::Identity(encoder_identity()),
        configuration: Box::new(Configuration {
            video_stream_format: amaterasu_module_sdk::VideoStreamFormat::H264_SDR,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate: 5_000_000,
            slices_per_frame: 1,
            qp: 28,
            min_threads: 2,
            max_reference_frames: 0,
            nvenc_settings: amaterasu_module_sdk::NvencSettings::default(),
            nvenc_spatial_aq: false,
            amd_av1_latency_mode: AmdAv1LatencyMode::Auto,
            sw_preset: None,
            sw_tune: None,
        }),
    }
}

fn open_configuration() -> Configuration {
    let Operation::Open { configuration, .. } = open() else {
        unreachable!()
    };
    *configuration
}

#[test]
fn software_options_reach_encoder_open_and_stream_status_unchanged() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    let mut operation = open();
    let Operation::Open {
        configuration,
        encoder,
        ..
    } = &mut operation
    else {
        unreachable!()
    };
    configuration.sw_preset = Some("ultrafast".into());
    configuration.sw_tune = Some("grain,zerolatency".into());
    *encoder = amaterasu_module_sdk::EncoderSelection::Identity(EncoderIdentity {
        clsid: "amaterasu.software.x264".into(),
        hardware_url: None,
        friendly_name: "x264 software H.264".into(),
    });
    pipeline.handle("alice", 99, operation, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    assert_eq!(pipeline.streams[&1].stage as u8, Stage::Running as u8);
    assert!(runtime.commands.iter().any(|command| matches!(command,
        Command::Request { service, payload, .. } if service == ENCODE
            && payload["encoder"]["clsid"] == "amaterasu.software.x264"
            && payload["configuration"]["sw_preset"] == "ultrafast"
            && payload["configuration"]["sw_tune"] == "grain,zerolatency"
    )));
    assert_eq!(
        runtime.completed[0].1["configuration"]["sw_preset"],
        "ultrafast"
    );
    assert_eq!(
        runtime.completed[0].1["configuration"]["sw_tune"],
        "grain,zerolatency"
    );
}

#[test]
fn required_reference_frame_limit_is_forwarded_and_confirmed() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    let mut operation = open();
    let Operation::Open { configuration, .. } = &mut operation else {
        unreachable!();
    };
    configuration.max_reference_frames = 3;
    pipeline.handle("alice", 99, operation, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    assert_eq!(pipeline.streams[&1].stage as u8, Stage::Running as u8);
    assert!(runtime.commands.iter().any(|command| matches!(
        command,
        Command::Request { service, payload, .. }
            if service == ENCODE
                && payload["configuration"]["max_reference_frames"] == 3
    )));
    assert_eq!(
        runtime.completed[0].1["configuration"]["max_reference_frames"],
        3
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

fn touch_surface() -> Value {
    json!({
        "width": 1920,
        "height": 1080,
        "logical_width": 1920,
        "logical_height": 1080
    })
}

fn portrait_geometry() -> CaptureGeometry {
    CaptureGeometry::new(
        DesktopRect::new(-768, 0, 768, 1024).unwrap(),
        DesktopRect::new(-768, 0, 2688, 1080).unwrap(),
    )
    .unwrap()
}

fn portrait_open(operation_id: &str) -> Operation {
    let mut request = open();
    let Operation::Open {
        configuration,
        operation_id: id,
        ..
    } = &mut request
    else {
        unreachable!();
    };
    configuration.width = 768;
    configuration.height = 1024;
    *id = Some(operation_id.into());
    request
}

#[test]
fn portrait_dxgi_touch_port_tracks_verified_geometry_through_lifecycle() {
    let geometry = portrait_geometry();
    let surface = json!({
        "width": 768, "height": 1024,
        "logical_width": 768, "logical_height": 1024
    });
    let (mut pipeline, mut runtime, mut plane) = setup();
    runtime.capture_geometry = Some(json!(geometry));
    runtime.capture_touch_surface = Some(surface.clone());
    pipeline.handle(
        "alice",
        99,
        portrait_open("portrait-dxgi"),
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    let opened = &runtime.completed[0].1;
    let port = json!({
        "width": 768, "height": 1024,
        "logical_width": 768, "logical_height": 1024,
        "client_offset_x": 0.0, "client_offset_y": 0.0
    });
    assert_eq!(opened["state"], "running");
    assert_eq!(opened["desktop_geometry"], json!(geometry));
    assert_eq!(opened["touch_port"], port);
    assert_eq!(plane.geometry_checks, vec![Some(geometry); 2]);

    // The provider's later state cannot change the stream's advertised map.
    runtime.capture_touch_surface = Some(json!({
        "width": 1024, "height": 768,
        "logical_width": 1024, "logical_height": 768
    }));
    pipeline.handle(
        "alice",
        100,
        Operation::Lookup {
            operation_id: "portrait-dxgi".into(),
        },
        &mut |command| runtime.send(command),
    );
    assert_eq!(runtime.completed.last().unwrap().1["touch_port"], port);
    pipeline.handle(
        "alice",
        101,
        Operation::Status { stream: Some(1) },
        &mut |command| runtime.send(command),
    );
    assert_eq!(
        runtime.completed.last().unwrap().1["streams"][0]["touch_port"],
        port
    );
    pipeline.handle(
        "bob",
        102,
        Operation::Lookup {
            operation_id: "portrait-dxgi".into(),
        },
        &mut |command| runtime.send(command),
    );
    assert_eq!(runtime.completed.last().unwrap().1["code"], "not_found");

    // Both the requested stream and verified physical output must match.
    for (request, output_width, stream_width) in [
        (portrait_open("stream-size-mismatch"), 768, 800),
        (portrait_open("output-size-mismatch"), 800, 768),
    ] {
        let (mut pipeline, mut runtime, mut plane) = setup();
        let output = CaptureGeometry::new(
            DesktopRect::new(-768, 0, output_width, 1024).unwrap(),
            geometry.virtual_desktop,
        )
        .unwrap();
        runtime.capture_geometry = Some(json!(output));
        runtime.capture_touch_surface = Some(surface.clone());
        let mut request = request;
        if let Operation::Open { configuration, .. } = &mut request {
            configuration.width = stream_width;
        }
        pipeline.handle("alice", 99, request, &mut |command| runtime.send(command));
        tick(&mut pipeline, &mut runtime, &mut plane, 4);
        assert_eq!(runtime.completed[0].1["state"], "running");
        assert!(runtime.completed[0].1.get("touch_port").is_none());
    }

    let (mut pipeline, mut runtime, mut plane) = setup();
    runtime.capture_geometry = Some(json!(geometry));
    runtime.capture_touch_surface = Some(json!({
        "width": 768, "height": 1024,
        "logical_width": 0, "logical_height": 1024
    }));
    pipeline.handle(
        "alice",
        99,
        portrait_open("malformed-portrait"),
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 12);
    assert_eq!(
        runtime.completed[0].1["code"],
        "invalid_capture_touch_surface"
    );
    assert!(runtime.commands.iter().any(|command| matches!(command,
        Command::Request {service,payload,..} if service == CAPTURE && payload["action"] == "close" && payload["stream"] == 41
    )));
    assert!(runtime.commands.iter().all(|command| !matches!(command,
        Command::Request {service,payload,..} if service == ENCODE && payload["action"] == "open"
    )));
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
    assert!(runtime.completed[0].1.get("touch_port").is_none());
}

#[test]
fn capture_touch_surface_produces_port_without_inference_from_desktop_geometry() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    runtime.capture_geometry = Some(json!(geometry()));
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);

    assert_eq!(
        runtime.completed[0].1["desktop_geometry"],
        json!(geometry())
    );
    assert!(runtime.completed[0].1.get("touch_port").is_none());

    let (mut pipeline, mut runtime, mut plane) = setup();
    runtime.capture_touch_surface = Some(touch_surface());
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    assert!(runtime.completed[0].1.get("touch_port").is_none());

    let (mut pipeline, mut runtime, mut plane) = setup();
    runtime.capture_geometry = Some(json!(geometry()));
    runtime.capture_touch_surface = Some(touch_surface());
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);

    assert_eq!(
        runtime.completed[0].1["touch_port"],
        json!({
            "width": 1920, "height": 1080,
            "logical_width": 1920, "logical_height": 1080,
            "client_offset_x": 0.0, "client_offset_y": 0.0
        })
    );
    assert_eq!(
        runtime.completed[0].1["desktop_geometry"],
        json!(geometry())
    );

    let (mut pipeline, mut runtime, mut plane) = setup();
    runtime.capture_geometry = Some(json!(geometry()));
    runtime.capture_touch_surface = Some(json!({
        "width": 1440, "height": 1080,
        "logical_width": 1440, "logical_height": 1080
    }));
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    assert!(runtime.completed[0].1.get("touch_port").is_none());
}

#[test]
fn capture_touch_surface_must_match_verified_output_rectangle() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    let mismatched_geometry = CaptureGeometry::new(
        DesktopRect::new(-1920, 0, 1600, 900).unwrap(),
        geometry().virtual_desktop,
    )
    .unwrap();
    runtime.capture_geometry = Some(json!(mismatched_geometry));
    runtime.capture_touch_surface = Some(touch_surface());
    let mut request = open();
    if let Operation::Open { operation_id, .. } = &mut request {
        *operation_id = Some("mismatched-touch-surface".into());
    }
    pipeline.handle("alice", 99, request, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);

    assert!(matches!(pipeline.streams[&1].stage, Stage::Running));
    assert_eq!(plane.geometry_checks, vec![Some(mismatched_geometry); 2]);
    assert_eq!(
        runtime.completed[0].1["desktop_geometry"],
        json!(mismatched_geometry)
    );
    assert!(runtime.completed[0].1.get("touch_port").is_none());

    pipeline.handle(
        "alice",
        100,
        Operation::Status { stream: Some(1) },
        &mut |command| runtime.send(command),
    );
    assert!(
        runtime.completed.last().unwrap().1["streams"][0]
            .get("touch_port")
            .is_none()
    );
    pipeline.handle(
        "alice",
        101,
        Operation::Lookup {
            operation_id: "mismatched-touch-surface".into(),
        },
        &mut |command| runtime.send(command),
    );
    assert!(
        runtime
            .completed
            .last()
            .unwrap()
            .1
            .get("touch_port")
            .is_none()
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    assert!(matches!(pipeline.streams[&1].stage, Stage::Running));
    assert!(pipeline.streams[&1].response(1).get("touch_port").is_none());
}

#[test]
fn malformed_capture_touch_surface_fails_before_encoder_open_and_closes_capture() {
    let valid = touch_surface();
    let mut malformed = Vec::new();
    for field in ["width", "height", "logical_width", "logical_height"] {
        let mut zero = valid.clone();
        zero[field] = json!(0);
        malformed.push(zero);
        let mut negative = valid.clone();
        negative[field] = json!(-1);
        malformed.push(negative);
        let mut too_large = valid.clone();
        too_large[field] = json!(u64::from(u32::MAX) + 1);
        malformed.push(too_large);
        let mut fractional = valid.clone();
        fractional[field] = json!(1.5);
        malformed.push(fractional);
    }
    for missing in ["width", "logical_height"] {
        let mut value = valid.clone();
        value.as_object_mut().unwrap().remove(missing);
        malformed.push(value);
    }
    let mut extra = valid.clone();
    extra["unexpected"] = json!(true);
    malformed.extend([extra, Value::Null, json!([])]);

    for touch_surface in malformed {
        let (mut pipeline, mut runtime, mut plane) = setup();
        runtime.capture_touch_surface = Some(touch_surface.clone());
        pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
        tick(&mut pipeline, &mut runtime, &mut plane, 12);
        assert_eq!(
            runtime.completed[0].1["code"], "invalid_capture_touch_surface",
            "{touch_surface}"
        );
        assert!(runtime.commands.iter().all(|command| !matches!(command,
            Command::Request { service, payload, .. }
                if service == ENCODE && payload["action"] == "open"
        )));
        assert!(runtime.commands.iter().any(|command| matches!(command,
            Command::Request { service, payload, .. }
                if service == CAPTURE && payload["action"] == "close" && payload["stream"] == 41
        )));
    }
}

#[test]
fn touch_port_is_immutable_after_capture_open() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    runtime.capture_geometry = Some(json!(geometry()));
    runtime.capture_touch_surface = Some(touch_surface());
    let mut request = open();
    if let Operation::Open { operation_id, .. } = &mut request {
        *operation_id = Some("touch-port".into());
    }
    pipeline.handle("alice", 99, request, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    let original = runtime.completed[0].1["touch_port"].clone();

    runtime.capture_touch_surface = Some(json!({
        "width": 800, "height": 600, "logical_width": 800, "logical_height": 600
    }));
    pipeline.handle(
        "alice",
        100,
        Operation::Status { stream: Some(1) },
        &mut |command| runtime.send(command),
    );
    assert_eq!(
        runtime.completed.last().unwrap().1["streams"][0]["touch_port"],
        original
    );
    pipeline.handle(
        "alice",
        101,
        Operation::Lookup {
            operation_id: "touch-port".into(),
        },
        &mut |command| runtime.send(command),
    );
    assert_eq!(runtime.completed.last().unwrap().1["touch_port"], original);
    tick(&mut pipeline, &mut runtime, &mut plane, 2);
    assert_eq!(pipeline.streams[&1].response(1)["touch_port"], original);
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
fn pipewire_open_reenumerates_one_current_source_and_keeps_its_id_private() {
    let output_id = "portal-private-output-id";
    let (mut pipeline, mut runtime, mut plane) =
        setup_with_providers([CaptureProvider::PipeWire].into());
    runtime.capture_enumeration_override = Some(pipewire_enumeration(pipewire_output(
        output_id, 1920, 1080, "monitor", false,
    )));
    pipeline.handle(
        "alice",
        99,
        canonical_open(
            "auto",
            CaptureProvider::PipeWire,
            "client-output-placeholder",
        ),
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 12);

    assert_eq!(pipeline.streams[&1].stage as u8, Stage::Running as u8);
    let provider_requests: Vec<_> = runtime
        .commands
        .iter()
        .filter_map(|command| match command {
            Command::Request {
                service, payload, ..
            } if service == CaptureProvider::PipeWire.service() => Some(payload.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(provider_requests[0]["action"], "enumerate");
    assert_eq!(provider_requests[1]["action"], "open");
    assert_eq!(provider_requests[1]["output"], output_id);
    assert_eq!(
        runtime.completed[0].1["output"],
        "client-output-placeholder"
    );
    assert!(!runtime.completed[0].1.to_string().contains(output_id));
}

#[test]
fn pipewire_requires_one_current_matching_geometry_before_open_without_fallback() {
    let (mut pipeline, mut runtime, mut plane) = setup_with_providers(
        [
            CaptureProvider::PipeWire,
            CaptureProvider::Dxgi,
            CaptureProvider::Wgc,
        ]
        .into(),
    );
    runtime.capture_enumeration_override = Some(json!({
        "ok": true,
        "outputs": [],
        "geometry_supported": false
    }));
    pipeline.handle(
        "alice",
        99,
        canonical_open(
            "auto",
            CaptureProvider::PipeWire,
            "client-output-placeholder",
        ),
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 12);
    assert_eq!(runtime.completed[0].1["code"], "source_selection_required");
    assert_eq!(plane.calls, 0);
    let capture_opens: Vec<_> = runtime
        .commands
        .iter()
        .filter_map(|command| match command {
            Command::Request {
                service, payload, ..
            } if capture_provider_for_service(service).is_some() && payload["action"] == "open" => {
                Some(service.as_str())
            }
            _ => None,
        })
        .collect();
    assert!(capture_opens.is_empty());
    assert_eq!(runtime.open_effects, 0);
    assert_eq!(alternate_provider(CaptureProvider::PipeWire), None);
    assert_eq!(
        output_device_name(CaptureProvider::PipeWire, "portal-source"),
        None
    );
}

#[test]
fn pipewire_enumeration_rejects_multiple_mismatched_and_unsupported_sources() {
    let configuration = open_configuration();
    let valid_output = pipewire_output("portal-1", 1920, 1080, "window", false);
    let valid_payload = pipewire_enumeration(valid_output.clone());
    assert_eq!(
        pipewire_selection_output(&valid_payload, &configuration).unwrap(),
        "portal-1"
    );

    assert_eq!(
        pipewire_selection_output(
            &json!({"ok":true,"outputs":[],"geometry_supported":false}),
            &configuration
        ),
        Err("source_selection_required")
    );
    assert_eq!(
        pipewire_selection_output(
            &json!({"ok":true,"outputs":[valid_output.clone(),valid_output.clone()],
                "enumeration_id":"portal-1","geometry_supported":false}),
            &configuration
        ),
        Err("invalid_capture_selection")
    );
    assert_eq!(
        pipewire_selection_output(
            &pipewire_enumeration(pipewire_output("portal-1", 1280, 720, "monitor", false)),
            &configuration
        ),
        Err("capture_dimensions_mismatch")
    );
    assert_eq!(
        pipewire_selection_output(
            &pipewire_enumeration(pipewire_output("portal-1", 1920, 1080, "monitor", true)),
            &configuration
        ),
        Err("invalid_capture_selection")
    );
    assert_eq!(
        pipewire_selection_output(
            &json!({"ok":true,"outputs":[valid_output],"enumeration_id":"stale-output",
                "geometry_supported":false}),
            &configuration
        ),
        Err("invalid_capture_selection")
    );
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
fn auto_fallback_checks_every_enumeration_page_before_opening() {
    let dxgi_id = format!("00000000:00000001/{}", r"\\.\DISPLAY1");
    let providers = [CaptureProvider::Dxgi, CaptureProvider::Wgc].into();
    let (mut pipeline, mut runtime, mut plane) = setup_with_providers(providers);
    runtime
        .capture_open_errors
        .insert(CaptureProvider::Dxgi, "unsupported_platform");
    runtime.capture_page_size = Some(1);
    runtime.capture_outputs.insert(
        CaptureProvider::Wgc,
        [r"\\.\DISPLAY0", r"\\.\DISPLAY1", r"\\.\DISPLAY2"]
            .into_iter()
            .map(|id| capture_output(id, id))
            .collect(),
    );

    let mut request = canonical_open("auto", CaptureProvider::Dxgi, &dxgi_id);
    if let Operation::Open { operation_id, .. } = &mut request {
        *operation_id = Some("paged-fallback".into());
    }
    pipeline.handle("alice", 99, request, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 18);

    assert!(matches!(pipeline.streams[&1].stage, Stage::Running));
    assert_eq!(pipeline.streams[&1].provider, CaptureProvider::Wgc);
    let alternate_requests: Vec<_> = runtime
        .commands
        .iter()
        .filter_map(|command| match command {
            Command::Request {
                service,
                payload,
                operation_id,
                ..
            } if service == CaptureProvider::Wgc.service() => Some((payload, operation_id)),
            _ => None,
        })
        .collect();
    assert_eq!(alternate_requests.len(), 4);
    assert_eq!(alternate_requests[0].0["action"], "enumerate");
    assert!(alternate_requests[0].0.get("offset").is_none());
    assert_eq!(alternate_requests[1].0["offset"], 1);
    assert_eq!(alternate_requests[2].0["offset"], 2);
    assert_eq!(alternate_requests[3].0["action"], "open");
    assert_eq!(alternate_requests[3].0["output"], r"\\.\DISPLAY1");
    assert_ne!(alternate_requests[0].1, alternate_requests[1].1);
    assert_ne!(alternate_requests[1].1, alternate_requests[2].1);

    let stream = &pipeline.streams[&1];
    let operation = stream.operation.clone();
    let capture_operation = stream.capture_operation.clone();
    assert_eq!(capture_operation, format!("{operation}-capture-1"));
    assert_eq!(alternate_requests[3].0["operation_id"], capture_operation);
    assert_eq!(
        alternate_requests[3].1,
        &format!("{operation}-capture-attempt-1")
    );

    pipeline.handle(
        "alice",
        100,
        Operation::Lookup {
            operation_id: "paged-fallback".into(),
        },
        &mut |command| runtime.send(command),
    );
    let lookup = &runtime.completed.last().unwrap().1;
    assert_eq!(lookup["state"], "running");
    assert_eq!(lookup["capture_provider"], "wgc");

    pipeline.handle(
        "alice",
        101,
        Operation::Close { stream: 1 },
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 8);
    assert!(pipeline.streams.is_empty());
    assert!(runtime.commands.iter().any(|command| matches!(
        command,
        Command::Request { service, payload, .. }
            if service == CaptureProvider::Wgc.service()
                && payload["action"] == "lookup"
                && payload["operation_id"] == capture_operation
    )));
    assert!(runtime.commands.iter().any(|command| matches!(
        command,
        Command::Request { service, payload, .. }
            if service == CaptureProvider::Wgc.service()
                && payload["action"] == "close"
                && payload["stream"] == 41
    )));
    assert!(!runtime.commands.iter().any(|command| matches!(
        command,
        Command::Request { service, payload, .. }
            if service == CaptureProvider::Dxgi.service()
                && matches!(payload["action"].as_str(), Some("lookup" | "close"))
    )));
}

#[test]
fn lost_later_fallback_page_settles_before_any_alternate_open() {
    let dxgi_id = format!("00000000:00000001/{}", r"\\.\DISPLAY1");
    let (mut pipeline, mut runtime, mut plane) =
        setup_with_providers([CaptureProvider::Dxgi, CaptureProvider::Wgc].into());
    runtime
        .capture_open_errors
        .insert(CaptureProvider::Dxgi, "unsupported_platform");
    runtime.capture_page_size = Some(1);
    runtime.capture_outputs.insert(
        CaptureProvider::Wgc,
        [r"\\.\DISPLAY0", r"\\.\DISPLAY1"]
            .into_iter()
            .map(|id| capture_output(id, id))
            .collect(),
    );

    pipeline.handle(
        "alice",
        99,
        canonical_open("auto", CaptureProvider::Dxgi, &dxgi_id),
        &mut |command| runtime.send(command),
    );
    for _ in 0..12 {
        if runtime.commands.iter().any(|command| {
            matches!(
                command,
                Command::Request { service, payload, .. }
                    if service == CaptureProvider::Wgc.service()
                        && payload["action"] == "enumerate"
                        && payload["offset"] == 1
            )
        }) {
            break;
        }
        tick(&mut pipeline, &mut runtime, &mut plane, 1);
    }
    assert!(runtime.commands.iter().any(|command| matches!(
        command,
        Command::Request { service, payload, .. }
            if service == CaptureProvider::Wgc.service()
                && payload["action"] == "enumerate"
                && payload["offset"] == 1
    )));
    runtime.lose_result = true;
    tick(&mut pipeline, &mut runtime, &mut plane, 12);

    assert!(pipeline.streams.is_empty());
    assert_eq!(pipeline.recent[0].1["error"], "request_failed");
    assert!(
        runtime
            .commands
            .iter()
            .any(|command| matches!(command, Command::Cancel { .. }))
    );
    assert!(!runtime.commands.iter().any(|command| matches!(
        command,
        Command::Request { service, payload, .. }
            if service == CaptureProvider::Wgc.service() && payload["action"] == "open"
    )));
}

#[test]
fn auto_fallback_rejects_matches_split_across_pages() {
    let wgc_id = r"\\.\DISPLAY1";
    let first = format!("00000000:00000001/{wgc_id}");
    let second = format!("00000000:00000002/{wgc_id}");
    let providers = [CaptureProvider::Dxgi, CaptureProvider::Wgc].into();
    let (mut pipeline, mut runtime, mut plane) = setup_with_providers(providers);
    runtime
        .capture_open_errors
        .insert(CaptureProvider::Wgc, "unsupported_platform");
    runtime.capture_page_size = Some(1);
    runtime.capture_outputs.insert(
        CaptureProvider::Dxgi,
        vec![
            capture_output(&first, wgc_id),
            capture_output(&second, wgc_id),
        ],
    );

    pipeline.handle(
        "alice",
        99,
        canonical_open("auto", CaptureProvider::Wgc, wgc_id),
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 18);

    assert!(pipeline.streams.is_empty());
    assert_eq!(pipeline.recent[0].1["error"], "unsupported_platform");
    assert!(!runtime.commands.iter().any(|command| matches!(
        command,
        Command::Request { service, payload, .. }
            if service == CaptureProvider::Dxgi.service() && payload["action"] == "open"
    )));
}

#[test]
fn malformed_fallback_pages_end_parent_without_opening_another_capture() {
    let monitor = r"\\.\DISPLAY1";
    let target = format!("00000000:00000001/{monitor}");
    let output = capture_output(&target, monitor);
    let token = "a".repeat(64);
    let malformed_pages = [
        json!({"ok":true}),
        json!({"ok":true,"outputs":{}}),
        json!({"ok":true,"outputs":[output],"next_offset":1,"enumeration_id":""}),
        json!({"ok":true,"outputs":[output],"next_offset":0,"enumeration_id":token}),
        json!({"ok":true,"outputs":[output],"next_offset":2,"enumeration_id":token}),
        json!({"ok":true,"outputs":[],"next_offset":1,"enumeration_id":token}),
        json!({"ok":true,"outputs":[output,output],"next_offset":2,"enumeration_id":token}),
    ];
    for page in malformed_pages {
        let (mut pipeline, mut runtime, mut plane) =
            setup_with_providers([CaptureProvider::Dxgi, CaptureProvider::Wgc].into());
        runtime
            .capture_open_errors
            .insert(CaptureProvider::Wgc, "unsupported_platform");
        runtime.capture_enumeration_override = Some(page.clone());
        pipeline.handle(
            "alice",
            99,
            canonical_open("auto", CaptureProvider::Wgc, monitor),
            &mut |command| runtime.send(command),
        );
        tick(&mut pipeline, &mut runtime, &mut plane, 20);

        assert!(pipeline.streams.is_empty(), "page: {page}");
        assert!(runtime.ended.contains(&99), "page: {page}");
        assert_eq!(runtime.open_effects, 1, "page: {page}");
        assert_eq!(pipeline.recent[0].1["error"], "unsupported_platform");
    }
}

#[test]
fn auto_fallback_rejects_hotplug_that_shifts_offset_paging() {
    let wgc_id = r"\\.\DISPLAY1";
    let id = |adapter: u32, device: &str| format!("00000000:{adapter:08x}/{device}");
    let first = capture_output(&id(1, r"\\.\DISPLAY0"), r"\\.\DISPLAY0");
    let match_one = capture_output(&id(2, wgc_id), wgc_id);
    let match_two = capture_output(&id(3, wgc_id), wgc_id);
    let last = capture_output(&id(4, r"\\.\DISPLAY2"), r"\\.\DISPLAY2");
    let providers = [CaptureProvider::Dxgi, CaptureProvider::Wgc].into();
    let (mut pipeline, mut runtime, mut plane) = setup_with_providers(providers);
    runtime
        .capture_open_errors
        .insert(CaptureProvider::Wgc, "unsupported_platform");
    runtime.capture_page_size = Some(1);
    runtime.capture_outputs.insert(
        CaptureProvider::Dxgi,
        vec![first, match_one.clone(), match_two.clone(), last.clone()],
    );
    // The first output disappears between pages. Offset 1 now points past
    // match_one; IDs still ascend, so only the full-list token catches it.
    runtime.capture_outputs_after_first_page = Some(vec![match_one, match_two, last]);

    pipeline.handle(
        "alice",
        99,
        canonical_open("auto", CaptureProvider::Wgc, wgc_id),
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 18);

    assert!(pipeline.streams.is_empty());
    assert_eq!(pipeline.recent[0].1["error"], "unsupported_platform");
    assert!(!runtime.commands.iter().any(|command| matches!(
        command,
        Command::Request { service, payload, .. }
            if service == CaptureProvider::Dxgi.service() && payload["action"] == "open"
    )));
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
fn rejected_forced_keyframe_closes_both_providers_and_reports_failure() {
    use amaterasu_media::{EncodedPacket, MEDIA_CLOSED, PacketSink};
    use amaterasu_module_sdk::abi::{STATUS_OK, Status};

    unsafe extern "C" fn accept(_: *mut std::ffi::c_void, _: *const EncodedPacket) -> Status {
        STATUS_OK
    }

    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    pipeline.handle(
        "alice",
        100,
        Operation::ForceIdr { stream: 1 },
        &mut |command| runtime.send(command),
    );
    assert_eq!(
        runtime.completed.last().unwrap().1["force_idr_pending"],
        true
    );
    assert!(
        runtime
            .completed
            .last()
            .unwrap()
            .1
            .get("keyframe")
            .is_none()
    );

    pipeline.streams.get_mut(&1).unwrap().next_frame = Instant::now();
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    assert!(plane.inputs.last().unwrap().2);
    assert!(!pipeline.streams[&1].force_idr);

    // The encoder can accept the forced input, then reject its non-IDR output
    // during a later receive. No successful keyframe is promised to the caller.
    plane.failure_code = Some("encoder_failed");
    pipeline.streams.get_mut(&1).unwrap().next_frame = Instant::now();
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    assert_eq!(pipeline.streams[&1].stage as u8, Stage::FindEncoder as u8);
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
        MEDIA_CLOSED
    );

    tick(&mut pipeline, &mut runtime, &mut plane, 8);
    assert!(pipeline.streams.is_empty());
    assert_eq!(pipeline.recent[0].1["state"], "failed");
    assert_eq!(pipeline.recent[0].1["error"], "encoder_failed");
    assert_eq!(
        runtime
            .commands
            .iter()
            .filter(|command| matches!(command,
                Command::Request {service, payload, parent: None, ..}
                    if service == ENCODE && payload["action"] == "close" && payload["session_id"] == 51
            ))
            .count(),
        1
    );
    assert_eq!(
        runtime
            .commands
            .iter()
            .filter(|command| matches!(command,
                Command::Request {service, payload, parent: None, ..}
                    if service == CAPTURE && payload["action"] == "close" && payload["stream"] == 41
            ))
            .count(),
        1
    );
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

#[test]
fn legacy_index_resolves_once_and_freezes_identity_before_allocation() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    let mut operation = open();
    let Operation::Open { encoder, .. } = &mut operation else {
        unreachable!()
    };
    *encoder = EncoderSelection::Index(0);
    pipeline.handle("alice", 99, operation, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 2);
    assert_eq!(runtime.open_effects, 0);
    runtime.encoder_enumeration = Some(json!({"ok":true,"enabled":true,"encoders":[]}));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    assert_eq!(runtime.commands.iter().filter(|command| matches!(command,
        Command::Request {service,payload,..} if service == ENCODE && payload["action"] == "enumerate")).count(), 1);
    let open_position = runtime
        .commands
        .iter()
        .position(|command| {
            matches!(command,
        Command::Request {service,payload,..} if service == CAPTURE && payload["action"] == "open")
        })
        .unwrap();
    let release_position = runtime
        .commands
        .iter()
        .position(|command| matches!(command, Command::ReleaseResult { request: 1 }))
        .unwrap();
    assert!(release_position < open_position);
    assert!(runtime.commands.iter().any(|command| matches!(command,
        Command::Request {service,payload,..} if service == ENCODE && payload["action"] == "open" && payload["encoder"] == json!(encoder_identity()))));
    let status = pipeline.streams.values().next().unwrap().response(1);
    assert_eq!(status["requested_encoder"], 0);
    assert_eq!(status["encoder"], json!(encoder_identity()));
}

#[test]
fn unavailable_selection_releases_enumeration_without_allocating_resources() {
    for (selection, reason) in [
        (EncoderSelection::Index(1), "encoder_unavailable"),
        (
            EncoderSelection::Backend(amaterasu_module_sdk::EncoderBackend::Software),
            "x264_feature_disabled",
        ),
    ] {
        let (mut pipeline, mut runtime, mut plane) = setup();
        runtime.encoder_enumeration = Some(
            json!({"ok":true,"enabled":true,"encoders":[],"software_encoder":{"available":false,"reason":"x264_feature_disabled"}}),
        );
        let mut operation = open();
        let Operation::Open { encoder, .. } = &mut operation else {
            unreachable!()
        };
        *encoder = selection;
        pipeline.handle("alice", 99, operation, &mut |command| runtime.send(command));
        tick(&mut pipeline, &mut runtime, &mut plane, 5);
        assert!(pipeline.streams.is_empty());
        assert_eq!(runtime.open_effects, 0);
        assert!(
            runtime
                .commands
                .iter()
                .any(|command| matches!(command, Command::ReleaseResult { request: 1 }))
        );
        assert_eq!(runtime.completed.last().unwrap().1["code"], reason);
    }
}

#[test]
fn auto_probes_all_hardware_before_software_and_freezes_the_usable_identity() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    let mut operation = auto_open();
    let Operation::Open { configuration, .. } = &mut operation else {
        unreachable!()
    };
    configuration.sw_preset = Some("ultrafast".into());
    configuration.sw_tune = Some("film,zerolatency".into());
    let frozen_configuration = json!(configuration);
    let software = candidate_description(amaterasu_module_sdk::SOFTWARE_ENCODER_ID, false);
    let first = candidate_description("first-hardware", true);
    let second = candidate_description("second-hardware", true);
    runtime.encoder_enumeration = Some(json!({"ok":true,"enabled":true,
        "encoders":[software,first,second]}));
    for candidate in [&first, &second] {
        runtime.encoder_probe_replies.insert(
            candidate["identity"]["clsid"].as_str().unwrap().into(),
            probe_reply(&candidate["identity"], &frozen_configuration, false),
        );
    }
    pipeline.handle("alice", 99, operation, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 18);
    let probes: Vec<_> = runtime
        .commands
        .iter()
        .filter_map(|command| match command {
            Command::Request {
                service, payload, ..
            } if service == ENCODE && payload["action"] == "probe" => Some(payload),
            _ => None,
        })
        .collect();
    assert_eq!(
        probes
            .iter()
            .map(|payload| payload["encoder"]["clsid"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "first-hardware",
            "second-hardware",
            amaterasu_module_sdk::SOFTWARE_ENCODER_ID
        ]
    );
    assert!(
        probes
            .iter()
            .all(|payload| payload["configuration"] == frozen_configuration)
    );
    let stream = &pipeline.streams[&1];
    assert!(stream.stage == Stage::Running);
    assert_eq!(json!(stream.encoder), software["identity"]);
    assert_eq!(json!(stream.requested_encoder), "auto");
    assert_eq!(stream.encoder_probe_attempt, 3);
    assert_eq!(runtime.open_effects, 2);
    let capture_position = runtime.commands.iter().position(|command|
        matches!(command, Command::Request {service,payload,..} if service == CAPTURE && payload["action"] == "open")).unwrap();
    let last_probe_id = runtime
        .operations
        .iter()
        .find(|(operation, _)| operation.ends_with("probe-encoder-3"))
        .unwrap()
        .1;
    let release_position = runtime.commands.iter().position(|command|
        matches!(command, Command::ReleaseResult {request} if request == last_probe_id)).unwrap();
    assert!(release_position < capture_position);
    runtime.encoder_enumeration = Some(json!({"ok":true,"enabled":true,"encoders":[]}));
    tick(&mut pipeline, &mut runtime, &mut plane, 2);
    assert_eq!(json!(pipeline.streams[&1].encoder), software["identity"]);
    assert_eq!(runtime.commands.iter().filter(|command| matches!(command,
        Command::Request {service,payload,..} if service == ENCODE && payload["action"] == "enumerate")).count(), 1);
}

#[test]
fn explicit_encoder_selection_never_probes_or_substitutes_another_candidate() {
    for selection in [
        EncoderSelection::Identity(encoder_identity()),
        EncoderSelection::Index(0),
    ] {
        let (mut pipeline, mut runtime, mut plane) = setup();
        runtime.encoder_open_error = Some("encoder_open_failed");
        let mut operation = open();
        let Operation::Open { encoder, .. } = &mut operation else {
            unreachable!()
        };
        *encoder = selection;
        pipeline.handle("alice", 99, operation, &mut |command| runtime.send(command));
        tick(&mut pipeline, &mut runtime, &mut plane, 20);
        assert!(!runtime.commands.iter().any(|command| matches!(command,
            Command::Request {service,payload,..} if service == ENCODE && payload["action"] == "probe")));
        assert!(pipeline.streams.is_empty());
        let attempts: Vec<_> = runtime
            .commands
            .iter()
            .filter_map(|command| match command {
                Command::Request {
                    service, payload, ..
                } if service == ENCODE && payload["action"] == "open" => Some(&payload["encoder"]),
                _ => None,
            })
            .collect();
        assert_eq!(attempts, [&json!(encoder_identity())]);
    }
}

#[test]
fn auto_rejects_uncorrelated_or_unsettled_probe_results_before_capture() {
    for mutation in [
        "configuration",
        "identity",
        "codec",
        "settled",
        "code",
        "transport_error",
    ] {
        let (mut pipeline, mut runtime, mut plane) = setup();
        let operation = auto_open();
        let Operation::Open { configuration, .. } = &operation else {
            unreachable!()
        };
        let mut reply = probe_reply(&json!(encoder_identity()), &json!(configuration), false);
        match mutation {
            "configuration" => reply["configuration"]["sw_tune"] = json!(""),
            "identity" => reply["encoder"]["hardware_url"] = json!("other-device"),
            "codec" => reply["codec"] = json!("av1"),
            "settled" => reply["settled"] = json!(false),
            "code" => reply["code"] = json!("permission_denied"),
            "transport_error" => reply = error("encoder_probe_cleanup_failed"),
            _ => unreachable!(),
        }
        runtime
            .encoder_probe_replies
            .insert(encoder_identity().clsid, reply);
        pipeline.handle("alice", 99, operation, &mut |command| runtime.send(command));
        tick(&mut pipeline, &mut runtime, &mut plane, 8);
        assert_eq!(runtime.open_effects, 0, "{mutation}");
        assert!(pipeline.streams.is_empty(), "{mutation}");
        assert_eq!(
            runtime.completed.last().unwrap().1["code"],
            "invalid_encoder_probe_reply"
        );
    }
}

#[test]
fn auto_probe_deadline_cancellation_uncertainty_and_result_release_gate_capture() {
    for boundary in ["deadline", "cancel", "uncertain", "release"] {
        let (mut pipeline, mut runtime, mut plane) = setup();
        runtime.hold_encoder_probes = true;
        pipeline.handle("alice", 99, auto_open(), &mut |command| {
            runtime.send(command)
        });
        tick(&mut pipeline, &mut runtime, &mut plane, 2);
        assert!(pipeline.streams[&1].stage == Stage::ProbeEncoder);
        if boundary == "uncertain" {
            runtime.lose_probe_accept = true;
        }
        tick(&mut pipeline, &mut runtime, &mut plane, 1);
        assert_eq!(runtime.open_effects, 0);
        match boundary {
            "deadline" => {
                pipeline.streams.get_mut(&1).unwrap().opened_at =
                    Instant::now() - Duration::from_secs(6)
            }
            "cancel" => runtime.cancelled = true,
            "release" => {
                runtime.hold_encoder_probes = false;
                runtime.release_failures = 10;
            }
            "uncertain" => {}
            _ => unreachable!(),
        }
        tick(&mut pipeline, &mut runtime, &mut plane, 6);
        assert_eq!(runtime.open_effects, 0, "{boundary}");
        assert_eq!(runtime.commands.iter().filter(|command| matches!(command,
            Command::Request {service,payload,..} if service == ENCODE && payload["action"] == "probe")).count(),
            if boundary == "uncertain" { 2 } else { 1 }, "{boundary}");
        if boundary == "release" {
            assert!(pipeline.streams[&1].unsettled.is_some());
        } else {
            assert!(pipeline.streams.is_empty(), "{boundary}");
        }
    }
}

#[test]
fn pending_selection_deadline_and_parent_cancellation_release_without_allocation() {
    for cancel_parent in [false, true] {
        let (mut pipeline, mut runtime, mut plane) = setup();
        runtime.hold_encoder_enumeration = true;
        let mut operation = open();
        let Operation::Open { encoder, .. } = &mut operation else {
            unreachable!()
        };
        *encoder = EncoderSelection::Index(0);
        pipeline.handle("alice", 99, operation, &mut |command| runtime.send(command));
        tick(&mut pipeline, &mut runtime, &mut plane, 1);
        if cancel_parent {
            runtime.cancelled = true;
        } else {
            pipeline.streams.values_mut().next().unwrap().opened_at =
                Instant::now() - Duration::from_secs(6);
        }
        tick(&mut pipeline, &mut runtime, &mut plane, 4);
        assert!(pipeline.streams.is_empty());
        assert_eq!(runtime.open_effects, 0);
        assert!(
            runtime
                .commands
                .iter()
                .any(|command| matches!(command, Command::Cancel { request: 1 }))
        );
        assert!(
            runtime
                .commands
                .iter()
                .any(|command| matches!(command, Command::ReleaseResult { request: 1 }))
        );
    }
}

#[test]
fn selection_release_failure_blocks_allocation_until_settled() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    let mut operation = open();
    let Operation::Open { encoder, .. } = &mut operation else {
        unreachable!()
    };
    *encoder = EncoderSelection::Index(0);
    pipeline.handle("alice", 99, operation, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 2);
    runtime.release_failures = 1;
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    assert_eq!(runtime.open_effects, 0);
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    assert_eq!(runtime.open_effects, 2);
    assert!(pipeline.streams.values().next().unwrap().stage == Stage::Running);
}

#[test]
fn uncertain_selection_admission_is_reconciled_and_never_allocates() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    runtime.lose_accept = true;
    let mut operation = open();
    let Operation::Open { encoder, .. } = &mut operation else {
        unreachable!()
    };
    *encoder = EncoderSelection::Index(0);
    pipeline.handle("alice", 99, operation, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 5);
    assert!(pipeline.streams.is_empty());
    assert_eq!(runtime.open_effects, 0);
    assert_eq!(runtime.operations.len(), 1);
    assert!(
        runtime
            .commands
            .iter()
            .any(|command| matches!(command, Command::ReleaseResult { request: 1 }))
    );
}
