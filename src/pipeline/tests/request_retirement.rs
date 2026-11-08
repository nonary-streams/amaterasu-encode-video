//! Exercise actual broker retention; the older provider fixture consumes reads.
use super::*;
use amaterasu_runtime::Broker;

const VIDEO_SERVICE: &str = "amaterasu.video/1";

struct BrokerFixture {
    broker: Broker,
    providers: Runtime,
    native_resources: BTreeMap<(String, String), u64>,
    children: BTreeMap<u64, (String, String)>,
    release_fault: Option<bool>,
    release_fault_target: (&'static str, &'static str),
    faulted_child: Option<u64>,
    faulted_release_attempts: usize,
    take_fault: Option<u64>,
    hold_capture_open: bool,
    held_capture_open: Option<(u64, String)>,
}

impl BrokerFixture {
    fn new() -> Self {
        let configuration = serde_json::from_value(json!({
            "principals": {
                "alice": {"call": [VIDEO_SERVICE, CAPTURE, ENCODE]},
                "video": {"provide": [VIDEO_SERVICE], "call": [CAPTURE, ENCODE]},
                "capture": {"provide": [CAPTURE]},
                "encoder": {"provide": [ENCODE]}
            }
        }))
        .unwrap();
        let mut broker = Broker::new(configuration).unwrap();
        for (principal, service) in [
            ("video", VIDEO_SERVICE),
            ("capture", CAPTURE),
            ("encoder", ENCODE),
        ] {
            assert!(matches!(
                broker.dispatch(
                    principal,
                    Command::Register {
                        service: service.into()
                    }
                ),
                Response::Ok
            ));
        }
        Self {
            broker,
            providers: Runtime::default(),
            native_resources: BTreeMap::new(),
            children: BTreeMap::new(),
            release_fault: None,
            release_fault_target: (CAPTURE, "open"),
            faulted_child: None,
            faulted_release_attempts: 0,
            take_fault: None,
            hold_capture_open: false,
            held_capture_open: None,
        }
    }

    fn begin(&mut self, pipeline: &mut Pipeline, serial: usize, operation: Operation) -> u64 {
        let response = self.broker.dispatch("alice", Command::Request {
            service: VIDEO_SERVICE.into(),
            payload: json!({"action": if matches!(operation, Operation::Close { .. }) {"close"} else {"open"}}),
            timeout_ms: 5000,
            operation_id: format!("fixture-{serial}"),
            parent: None,
        });
        let Response::Accepted { id } = response else {
            panic!("broker admission failed at operation {serial}: {response:?}");
        };
        assert!(matches!(
            self.broker.dispatch("video", Command::Poll {}),
            Response::Request { id: delivered, .. } if delivered == id
        ));
        pipeline.handle("alice", id, operation, &mut |command| self.send(command));
        id
    }

    fn release_parent(&mut self, parent: u64) {
        assert!(matches!(
            self.broker
                .dispatch("alice", Command::TakeResult { request: parent }),
            Response::Result { .. }
        ));
        assert!(matches!(
            self.broker
                .dispatch("alice", Command::ReleaseResult { request: parent }),
            Response::Ok
        ));
    }

    fn send(&mut self, command: Command) -> amaterasu_module_sdk::ModuleResult<Response> {
        if let Command::TakeResult { request } = command
            && self.take_fault == Some(request)
        {
            self.take_fault = None;
            return Err("fixture lost result response".into());
        }
        if let Command::ReleaseResult { request } = command {
            if self.faulted_child == Some(request) {
                self.faulted_release_attempts += 1;
            } else if self.release_fault.is_some()
                && self
                    .children
                    .get(&request)
                    .is_some_and(|(service, action)| {
                        service == self.release_fault_target.0
                            && action == self.release_fault_target.1
                    })
            {
                self.faulted_child = Some(request);
                self.faulted_release_attempts = 1;
                if self.release_fault.take().unwrap() {
                    assert!(matches!(
                        self.broker.dispatch("video", command),
                        Response::Ok
                    ));
                }
                return Err("fixture lost result release reply".into());
            }
        }
        let response = self.broker.dispatch("video", command.clone());
        if let Command::Request {
            ref service,
            ref payload,
            ..
        } = command
            && let Response::Accepted { id } = response
        {
            self.children.insert(
                id,
                (service.clone(), payload["action"].as_str().unwrap().into()),
            );
            let provider = if service == CAPTURE {
                "capture"
            } else {
                "encoder"
            };
            match self.broker.dispatch(provider, Command::Poll {}) {
                Response::Request { id: delivered, .. } => {
                    assert_eq!(delivered, id);
                    if self.hold_capture_open && service == CAPTURE && payload["action"] == "open" {
                        self.held_capture_open =
                            Some((id, payload["operation_id"].as_str().unwrap().into()));
                        return Ok(response);
                    }
                    // Reuse only the provider's payload generator. Caller reads,
                    // completion, cancellation and release use the actual broker.
                    let Response::Accepted { id: generated } =
                        self.providers.send(command.clone())?
                    else {
                        panic!("fixture provider rejected its request");
                    };
                    let mut result = self.providers.pending.remove(&generated).unwrap();
                    let resource_field = if service == CAPTURE {
                        "stream"
                    } else {
                        "session_id"
                    };
                    match payload["action"].as_str().unwrap() {
                        "open" => {
                            self.native_resources.insert(
                                (
                                    service.clone(),
                                    payload["operation_id"].as_str().unwrap().into(),
                                ),
                                result[resource_field].as_u64().unwrap(),
                            );
                        }
                        "lookup" => {
                            let key = (
                                service.clone(),
                                payload["operation_id"].as_str().unwrap().into(),
                            );
                            result = if let Some(resource) = self.native_resources.get(&key) {
                                let mut found = json!({"ok":true});
                                found[resource_field] = json!(resource);
                                found
                            } else {
                                error("not_found")
                            };
                        }
                        "close" => {
                            self.native_resources.retain(|(provider, _), resource| {
                                provider != service
                                    || Some(*resource) != payload[resource_field].as_u64()
                            });
                        }
                        _ => {}
                    }
                    assert!(matches!(
                        self.broker.dispatch(
                            provider,
                            Command::Complete {
                                request: id,
                                payload: result
                            }
                        ),
                        Response::Ok
                    ));
                }
                Response::Pending => {} // An exact retry has already completed.
                other => panic!("unexpected provider poll: {other:?}"),
            }
        }
        Ok(response)
    }

    fn tick(&mut self, pipeline: &mut Pipeline, plane: &mut FakePlane, count: usize) {
        for _ in 0..count {
            pipeline.tick(plane, &mut |command| self.send(command));
        }
    }
}

#[test]
fn transient_effect_result_failure_retries_before_cleanup_lookup() {
    let (mut pipeline, _, mut plane) = setup();
    let mut fixture = BrokerFixture::new();
    let parent = fixture.begin(&mut pipeline, 0, open());
    for _ in 0..16 {
        fixture.tick(&mut pipeline, &mut plane, 1);
        if pipeline.streams[&1].stage == Stage::Capture && pipeline.streams[&1].pending.is_some() {
            break;
        }
    }
    let child = pipeline.streams[&1].pending.expect("capture open admitted");
    let close = fixture.begin(&mut pipeline, 1, Operation::Close { stream: 1 });
    fixture.release_parent(close);
    fixture.take_fault = Some(child);
    fixture.tick(&mut pipeline, &mut plane, 1);
    assert_eq!(pipeline.streams[&1].unsettled, Some(child));
    assert_ne!(pipeline.streams[&1].stage, Stage::Failed);
    fixture.tick(&mut pipeline, &mut plane, 20);
    assert!(pipeline.streams.is_empty());
    assert!(fixture.native_resources.is_empty());
    fixture.release_parent(parent);
}

#[test]
fn completed_control_requests_do_not_exhaust_the_real_broker() {
    let (mut pipeline, _, mut plane) = setup();
    let mut fixture = BrokerFixture::new();
    // Six retained effect/cleanup results per cycle exhaust the actual 512-slot
    // ledger before this finishes unless Video releases its successful reads.
    for cycle in 0..110 {
        let parent = fixture.begin(&mut pipeline, cycle * 2, open());
        fixture.tick(&mut pipeline, &mut plane, 16);
        let stream = pipeline.next_id;
        assert_eq!(
            pipeline.streams.get(&stream).map(|value| value.stage),
            Some(Stage::Running),
            "open stalled during broker-backed cycle {cycle}"
        );
        fixture.release_parent(parent);
        let close = fixture.begin(&mut pipeline, cycle * 2 + 1, Operation::Close { stream });
        fixture.release_parent(close);
        fixture.tick(&mut pipeline, &mut plane, 20);
        assert!(
            pipeline.streams.is_empty(),
            "cleanup stalled at cycle {cycle}"
        );
        assert!(fixture.native_resources.is_empty());
    }
    assert_eq!(fixture.providers.open_effects, 220);
    for (request, (service, action)) in &fixture.children {
        assert!(
            matches!(
                fixture.broker.dispatch("video", Command::TakeResult { request: *request }),
                Response::Error { code, .. } if code == "not_found"
            ),
            "retained successful provider result: {service} {action} request={request}"
        );
    }
}

#[test]
fn lost_successful_release_retries_the_ledger_without_replaying_open() {
    for after_commit in [false, true] {
        let (mut pipeline, _, mut plane) = setup();
        let mut fixture = BrokerFixture::new();
        fixture.release_fault = Some(after_commit);
        let parent = fixture.begin(&mut pipeline, 0, open());
        fixture.tick(&mut pipeline, &mut plane, 16);
        assert_eq!(fixture.faulted_release_attempts, 2);
        assert_eq!(fixture.providers.open_effects, 2);
        assert_eq!(pipeline.streams[&1].stage, Stage::Running);
        fixture.release_parent(parent);
        let close = fixture.begin(&mut pipeline, 1, Operation::Close { stream: 1 });
        fixture.release_parent(close);
        fixture.tick(&mut pipeline, &mut plane, 20);
        assert!(pipeline.streams.is_empty());
        assert!(fixture.native_resources.is_empty());
    }
}

#[test]
fn runtime_adapter_retains_open_result_when_release_ack_is_lost() {
    use amaterasu_module_sdk::RuntimeClient;
    use amaterasu_module_sdk::abi::{
        AbiHeader, BorrowedBytes, HostApi, ReplySink, STATUS_INTERNAL_ERROR, STATUS_OK, Status,
    };
    use std::ffi::c_void;

    unsafe extern "C" fn invoke(
        context: *mut c_void,
        input: BorrowedBytes,
        reply: ReplySink,
    ) -> Status {
        // SAFETY: the boxed fixture and callback buffers outlive each synchronous call.
        let fixture = unsafe { &mut *context.cast::<BrokerFixture>() };
        let command: Command = serde_json::from_slice(unsafe { input.read().unwrap() }).unwrap();
        let Ok(response) = fixture.send(command) else {
            return STATUS_INTERNAL_ERROR;
        };
        // SAFETY: the reply sink copies the serialized response synchronously.
        unsafe { reply.send(&serde_json::to_vec(&response).unwrap()) };
        STATUS_OK
    }

    for after_commit in [false, true] {
        let (mut pipeline, _, mut plane) = setup();
        let mut fixture = Box::new(BrokerFixture::new());
        fixture.release_fault = Some(after_commit);
        let parent = fixture.begin(&mut pipeline, 0, open());
        let endpoint = HostApi {
            header: AbiHeader::of::<HostApi>(),
            context: (&mut *fixture as *mut BrokerFixture).cast(),
            invoke,
        };
        // SAFETY: fixture remains boxed and alive for all endpoint calls.
        let runtime = unsafe { RuntimeClient::from_endpoint(endpoint) };
        for _ in 0..16 {
            pipeline.tick(&mut plane, &mut |command| {
                crate::send_pipeline_command(&runtime, command)
            });
        }
        assert_eq!(fixture.faulted_release_attempts, 2);
        assert_eq!(fixture.providers.open_effects, 2);
        assert_eq!(pipeline.streams[&1].stage, Stage::Running);
        fixture.release_parent(parent);
        let close = fixture.begin(&mut pipeline, 1, Operation::Close { stream: 1 });
        fixture.release_parent(close);
        for _ in 0..20 {
            pipeline.tick(&mut plane, &mut |command| {
                crate::send_pipeline_command(&runtime, command)
            });
        }
        assert!(pipeline.streams.is_empty());
        assert!(fixture.native_resources.is_empty());
    }
}

#[test]
fn parent_cancellation_during_result_release_keeps_exact_native_cleanup() {
    for after_commit in [false, true] {
        let (mut pipeline, _, mut plane) = setup();
        let mut fixture = BrokerFixture::new();
        fixture.release_fault = Some(after_commit);
        let parent = fixture.begin(&mut pipeline, 0, open());
        for _ in 0..16 {
            fixture.tick(&mut pipeline, &mut plane, 1);
            if fixture.faulted_child.is_some() {
                break;
            }
        }
        assert!(pipeline.streams[&1].completed_payload.is_some());
        assert_eq!(fixture.providers.open_effects, 1);
        assert!(matches!(
            fixture
                .broker
                .dispatch("alice", Command::Cancel { request: parent }),
            Response::Ok
        ));
        fixture.tick(&mut pipeline, &mut plane, 24);
        assert!(pipeline.streams.is_empty());
        assert!(fixture.native_resources.is_empty());
        assert_eq!(fixture.providers.open_effects, 1);
        assert!(fixture.providers.commands.iter().any(|command| {
            matches!(command, Command::Request { service, payload, .. }
                if service == CAPTURE && payload["action"] == "close" && payload["stream"] == 41)
        }));
        assert!(matches!(
            fixture
                .broker
                .dispatch("alice", Command::ReleaseResult { request: parent }),
            Response::Ok
        ));
        for request in fixture.children.keys() {
            assert!(matches!(
                fixture.broker.dispatch("video", Command::TakeResult { request: *request }),
                Response::Error { code, .. } if code == "not_found"
            ));
        }
    }
}

#[test]
fn cancelled_parent_does_not_retire_while_delivered_capture_open_can_finish_late() {
    let (mut pipeline, _, mut plane) = setup();
    let mut fixture = BrokerFixture::new();
    fixture.hold_capture_open = true;
    let parent = fixture.begin(&mut pipeline, 0, open());
    for _ in 0..16 {
        fixture.tick(&mut pipeline, &mut plane, 1);
        if fixture.held_capture_open.is_some() {
            break;
        }
    }
    let (child, operation) = fixture
        .held_capture_open
        .clone()
        .expect("capture open delivered");
    assert!(matches!(
        fixture
            .broker
            .dispatch("alice", Command::Cancel { request: parent }),
        Response::Ok
    ));
    fixture.tick(&mut pipeline, &mut plane, 24);

    assert!(
        pipeline.streams.contains_key(&1),
        "cancellation does not prove native retirement"
    );
    assert!(
        !fixture
            .children
            .values()
            .any(|(service, action)| service == CAPTURE && action == "lookup"),
        "lookup cannot prove absence while the delivered open is still running"
    );

    // A concurrent replacement provider may finish its allocation after the
    // cancelled child has become terminal. Broker completion cannot convey it.
    fixture
        .native_resources
        .insert((CAPTURE.into(), operation), 41);
    assert!(matches!(
        fixture.broker.dispatch(
            "capture",
            Command::Complete {
                request: child,
                payload: json!({"ok": true, "stream": 41})
            }
        ),
        Response::Error { .. }
    ));
    fixture.tick(&mut pipeline, &mut plane, 8);
    assert!(
        pipeline.streams.contains_key(&1),
        "unknown late effect must stay fenced"
    );
    assert_eq!(fixture.native_resources.len(), 1);
}

#[test]
fn quiescence_waits_for_delivered_capture_before_lookup_and_close() {
    let (mut pipeline, _, mut plane) = setup();
    let mut fixture = BrokerFixture::new();
    fixture.hold_capture_open = true;
    let parent = fixture.begin(&mut pipeline, 0, open());
    for _ in 0..16 {
        fixture.tick(&mut pipeline, &mut plane, 1);
        if fixture.held_capture_open.is_some() {
            break;
        }
    }
    let (child, operation) = fixture.held_capture_open.take().expect("open delivered");
    assert!(
        !pipeline
            .quiesce(&mut plane, &mut |command| fixture.send(command))
            .unwrap()
    );
    fixture.tick(&mut pipeline, &mut plane, 5);
    assert!(pipeline.streams.contains_key(&1));
    assert!(
        !fixture
            .children
            .values()
            .any(|(service, action)| service == CAPTURE && action == "lookup")
    );
    fixture
        .native_resources
        .insert((CAPTURE.into(), operation), 41);
    assert!(matches!(
        fixture.broker.dispatch(
            "capture",
            Command::Complete {
                request: child,
                payload: json!({"ok":true,"stream":41})
            }
        ),
        Response::Ok
    ));
    fixture.hold_capture_open = false;
    fixture.tick(&mut pipeline, &mut plane, 24);
    assert!(pipeline.streams.is_empty());
    assert!(fixture.native_resources.is_empty());
    assert!(
        pipeline
            .quiesce(&mut plane, &mut |command| fixture.send(command))
            .unwrap()
    );
    fixture.release_parent(parent);
}

#[test]
fn successful_final_close_waits_for_ledger_release_before_stream_retirement() {
    for after_commit in [false, true] {
        let (mut pipeline, _, mut plane) = setup();
        let mut fixture = BrokerFixture::new();
        let parent = fixture.begin(&mut pipeline, 0, open());
        fixture.tick(&mut pipeline, &mut plane, 16);
        fixture.release_parent(parent);
        fixture.release_fault = Some(after_commit);
        fixture.release_fault_target = (CAPTURE, "close");
        let close = fixture.begin(&mut pipeline, 1, Operation::Close { stream: 1 });
        fixture.release_parent(close);
        for _ in 0..16 {
            fixture.tick(&mut pipeline, &mut plane, 1);
            if fixture.faulted_child.is_some() {
                break;
            }
        }
        assert_eq!(pipeline.streams[&1].stage, Stage::CloseCapture);
        assert!(pipeline.streams[&1].completed_payload.is_some());
        fixture.tick(&mut pipeline, &mut plane, 4);
        assert!(pipeline.streams.is_empty());
        assert!(fixture.native_resources.is_empty());
        assert_eq!(fixture.faulted_release_attempts, 2);
        assert_eq!(fixture.providers.open_effects, 2);
        assert_eq!(
            fixture
                .providers
                .commands
                .iter()
                .filter(|command| {
                    matches!(command, Command::Request { service, payload, .. }
                if service == CAPTURE && payload["action"] == "close")
                })
                .count(),
            1
        );
    }
}
