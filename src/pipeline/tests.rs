use super::*;
use crate::model::EncoderIdentity;
use crate::plane::{GeometryProgress, Progress};
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
                self.next += 1;
                self.operations.insert(operation_id, self.next);
                if payload["action"] == "open" {
                    self.open_effects += 1;
                }
                let action = payload["action"].as_str().unwrap();
                let result = if self.fail_encoder_cleanup
                    && service == ENCODE
                    && matches!(action, "lookup" | "close")
                {
                    error("hardware_error")
                } else if service == CAPTURE || service == "amaterasu.capture-wgc/1" {
                    let mut result = json!({"ok":true,"stream":41});
                    if let Some(geometry) = &self.capture_geometry {
                        result["desktop_geometry"] = geometry.clone();
                    }
                    result
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

#[derive(Default)]
struct FakePlane {
    calls: usize,
    drains: usize,
    inputs: Vec<(CaptureProvider, bool, bool)>,
    dropped: bool,
    fail: bool,
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
        self.inputs
            .push((input.provider, input.capture_due, input.force_idr));
        if self.fail {
            Err("frame_rejected")
        } else {
            Ok(Progress {
                dropped: self.dropped,
                submitted: input.capture_due && !self.dropped,
                frame: None,
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
    (
        Pipeline::new(
            Settings {
                enabled: true,
                ..Settings::default()
            },
            "test".into(),
            Arc::new(Mutex::new(Queues::default())),
        ),
        Runtime::default(),
        FakePlane::default(),
    )
}

fn open() -> Operation {
    Operation::Open {
        packet_boundaries: false,
        operation_id: None,
        capture_provider: CaptureProvider::Dxgi,
        constant_rate: false,
        output: "display-1".into(),
        encoder: encoder_identity(),
        configuration: Configuration {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate: 5_000_000,
            max_reference_frames: 0,
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
    assert!(runtime.commands.iter().any(|command| matches!(
        command,
        Command::Request { service, payload, .. }
            if service == "amaterasu.capture-wgc/1"
                && payload["action"] == "open"
                && payload["constant_rate"] == true
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
fn provider_allowlist_controls_native_selection_and_every_cleanup_call() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    let wgc = || match open() {
        Operation::Open {
            output,
            encoder,
            configuration,
            constant_rate,
            ..
        } => Operation::Open {
            packet_boundaries: false,
            operation_id: None,
            capture_provider: CaptureProvider::Wgc,
            constant_rate,
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
            .all(|(_, capture, force)| !*capture && !*force)
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
