use super::*;
use crate::worker::{ReceiveResult, SubmitResult};
use crate::{
    factory::Factory,
    model::{Configuration, EncoderDescription, EncoderIdentity, Packet},
    sessions::{Backend, Sessions},
};
use std::sync::{Arc, Mutex, Weak};

#[derive(Default)]
struct HostState {
    incoming: Option<Response>,
    completed: Option<Value>,
    registered_factory: usize,
    registrations: usize,
}

pub(super) struct Host {
    state: Box<Mutex<HostState>>,
}

unsafe extern "C" fn dispatch(
    context: *mut c_void,
    request: BorrowedBytes,
    reply: ReplySink,
) -> Status {
    // SAFETY: Host owns the stable mutex, and the runtime borrows request bytes
    // and the reply sink only for this synchronous invocation.
    let state = unsafe { &*context.cast::<Mutex<HostState>>() };
    let command: Command = serde_json::from_slice(unsafe { request.read().unwrap() }).unwrap();
    let mut state = state.lock().unwrap();
    let response = match command {
        Command::GetSettings {} => Response::Result {
            payload: json!({"enabled":true}),
        },
        Command::Register { service } => {
            assert_eq!(service, SERVICE);
            Response::Ok
        }
        Command::NativeRegister { service, factory } => {
            assert_eq!(service, SERVICE);
            state.registered_factory = factory;
            state.registrations += 1;
            Response::Ok
        }
        Command::Poll {} => state.incoming.take().unwrap_or(Response::Pending),
        Command::CheckRequest { request: 42 } => Response::Ok,
        Command::Complete {
            request: 42,
            payload,
        } => {
            state.completed = Some(payload);
            Response::Ok
        }
        _ => panic!("unexpected acceptance-test runtime command"),
    };
    // SAFETY: serialized response storage outlives the synchronous sink call.
    unsafe { reply.send(&serde_json::to_vec(&response).unwrap()) };
    STATUS_OK
}

impl Host {
    pub(super) fn start() -> (Self, EncodeModule) {
        let host = Self {
            state: Box::default(),
        };
        let endpoint = HostApi {
            header: AbiHeader::of::<HostApi>(),
            context: (&*host.state as *const Mutex<HostState>).cast_mut().cast(),
            invoke: dispatch,
        };
        // SAFETY: the boxed endpoint state is retained until after module drop.
        let client = unsafe { RuntimeClient::from_endpoint(endpoint) };
        let module = EncodeModule::start(client).unwrap();
        host.assert_registration(&module);
        (host, module)
    }

    pub(super) fn request(&self, module: &mut EncodeModule, payload: Value) -> Value {
        {
            let mut state = self.state.lock().unwrap();
            state.completed = None;
            state.incoming = Some(Response::Request {
                id: 42,
                caller: "test.video".into(),
                initiator: "untrusted-request-initiator".into(),
                service: SERVICE.into(),
                remaining_ms: 5_000,
                payload,
            });
        }
        module.tick().unwrap();
        self.assert_registration(module);
        self.state.lock().unwrap().completed.take().unwrap()
    }

    pub(super) fn assert_registration(&self, module: &EncodeModule) {
        let state = self.state.lock().unwrap();
        assert_eq!(state.registrations, 1);
        assert_eq!(
            state.registered_factory,
            &module._registration.factory as *const NativeServiceFactory as usize
        );
    }
}

pub(super) fn hardware_identity() -> EncoderIdentity {
    EncoderIdentity {
        clsid: "test.registered.hardware".into(),
        hardware_url: Some("test-only-registered-device".into()),
        friendly_name: "Fault-registered hardware H.264".into(),
    }
}

pub(super) fn configuration() -> Configuration {
    serde_json::from_value(json!({
        "width":64,"height":48,"fps":30,"bitrate":1_000_000,
        "max_reference_frames":1
    }))
    .unwrap()
}

pub(super) fn probe_request(identity: &EncoderIdentity, config: &Configuration) -> Value {
    json!({"action":"probe","encoder":identity,"configuration":config})
}

pub(super) fn open_request(identity: &EncoderIdentity, config: &Configuration) -> Value {
    json!({
        "action":"open","encoder":identity,"configuration":config,
        "operation_id":"winner-open","packet_boundaries":true
    })
}

pub(super) fn assert_no_session(module: &EncodeModule) {
    let sessions = module.shared.lock().unwrap();
    assert_eq!(sessions.latest_id(), 0);
    assert!(matches!(
        sessions.lookup("test.video", "winner-open"),
        Err("session_not_found")
    ));
}

pub(super) fn assert_probe_reply(
    reply: &Value,
    identity: &EncoderIdentity,
    config: &Configuration,
    usable: bool,
) {
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["usable"], usable, "{reply}");
    assert_eq!(reply["settled"], true, "{reply}");
    assert_eq!(reply["codec"], "h264");
    assert_eq!(reply["encoder"], json!(identity));
    assert_eq!(reply["configuration"], json!(config));
    assert!(reply.get("session_id").is_none());
}

pub(super) struct Attempt {
    pub(super) identity: EncoderIdentity,
    pub(super) configuration: Configuration,
    pub(super) shared_lock_available: bool,
}

#[derive(Clone)]
pub(super) struct CapturedPacket {
    pub(super) bytes: Vec<u8>,
    pub(super) timestamp: i64,
    pub(super) keyframe: bool,
}

#[derive(Default)]
pub(super) struct Observations {
    pub(super) enumerations: usize,
    pub(super) attempts: Vec<Attempt>,
    pub(super) submitted: usize,
    pub(super) drained: usize,
    pub(super) closed: usize,
    pub(super) packets: Vec<CapturedPacket>,
}

pub(super) enum SoftwareMode {
    Controlled,
    #[cfg(amaterasu_x264)]
    Native,
}

pub(super) struct AcceptanceFactory {
    shared: Weak<Mutex<Sessions>>,
    observations: Arc<Mutex<Observations>>,
    software: EncoderDescription,
    mode: SoftwareMode,
}

impl AcceptanceFactory {
    pub(super) fn install(
        module: &EncodeModule,
        mode: SoftwareMode,
    ) -> (Arc<Mutex<Observations>>, EncoderIdentity) {
        let software = match mode {
            SoftwareMode::Controlled => EncoderDescription {
                identity: EncoderIdentity {
                    clsid: "test.controlled.probe-output".into(),
                    hardware_url: None,
                    friendly_name: "Controlled test output".into(),
                },
                name: "Controlled test output".into(),
                codec: "h264",
                hardware: false,
                vendor_id: None,
                capabilities: Default::default(),
            },
            #[cfg(amaterasu_x264)]
            SoftwareMode::Native => crate::factory::NativeFactory
                .enumerate()
                .unwrap()
                .into_iter()
                .find(|description| {
                    description.identity.clsid == amaterasu_module_sdk::SOFTWARE_ENCODER_ID
                        && !description.hardware
                })
                .expect("matching system x264 runtime is required for this qualification"),
        };
        let identity = software.identity.clone();
        let observations = Arc::new(Mutex::new(Observations::default()));
        let factory = Arc::new(Self {
            shared: Arc::downgrade(&module.shared),
            observations: observations.clone(),
            software,
            mode,
        });
        *module.shared.lock().unwrap() = Sessions::with_factory(true, factory);
        (observations, identity)
    }
}

impl Factory for AcceptanceFactory {
    fn enumerate(&self) -> Result<Vec<EncoderDescription>, &'static str> {
        self.observations.lock().unwrap().enumerations += 1;
        Ok(vec![
            EncoderDescription {
                identity: hardware_identity(),
                name: "Fault-registered hardware H.264".into(),
                codec: "h264",
                hardware: true,
                vendor_id: Some(0x10de),
                capabilities: Default::default(),
            },
            self.software.clone(),
        ])
    }

    fn open(
        &self,
        identity: EncoderIdentity,
        config: Configuration,
    ) -> Result<Box<dyn Backend>, &'static str> {
        let shared = self.shared.upgrade().unwrap();
        let shared_lock_available = shared.try_lock().is_ok();
        self.observations.lock().unwrap().attempts.push(Attempt {
            identity: identity.clone(),
            configuration: config.clone(),
            shared_lock_available,
        });
        if identity == hardware_identity() {
            return Err("encoder_open_failed");
        }
        if identity != self.software.identity {
            return Err("encoder_identity_changed");
        }
        let backend: Box<dyn Backend> = match self.mode {
            SoftwareMode::Controlled => Box::new(ControlledBackend { packet: None }),
            #[cfg(amaterasu_x264)]
            SoftwareMode::Native => crate::factory::NativeFactory.open(identity, config)?,
        };
        Ok(Box::new(RecordingBackend {
            backend: Some(backend),
            observations: self.observations.clone(),
        }))
    }
}

struct ControlledBackend {
    packet: Option<Packet>,
}

impl Backend for ControlledBackend {
    fn submit(
        &mut self,
        _: Vec<u8>,
        timestamp: i64,
        force: bool,
    ) -> Result<SubmitResult, &'static str> {
        assert!(force);
        self.packet = Some(Packet {
            bytes: vec![
                0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 1, 0, 0, 0, 1, 0x65, 1,
            ],
            timestamp_100ns: timestamp,
            keyframe: true,
        });
        Ok(SubmitResult::Accepted)
    }

    fn receive(&mut self) -> Result<ReceiveResult, &'static str> {
        Ok(ReceiveResult::Ready(self.packet.take()))
    }

    fn drain(&mut self) -> Result<bool, &'static str> {
        Ok(true)
    }
}

struct RecordingBackend {
    backend: Option<Box<dyn Backend>>,
    observations: Arc<Mutex<Observations>>,
}

impl Backend for RecordingBackend {
    fn submit(
        &mut self,
        bytes: Vec<u8>,
        timestamp: i64,
        force: bool,
    ) -> Result<SubmitResult, &'static str> {
        let pixels = bytes.len() * 2 / 3;
        assert!(bytes[..pixels].iter().all(|value| *value == 16));
        assert!(bytes[pixels..].iter().all(|value| *value == 128));
        assert!(force);
        let result = self
            .backend
            .as_mut()
            .unwrap()
            .submit(bytes, timestamp, force)?;
        self.observations.lock().unwrap().submitted += 1;
        Ok(result)
    }

    fn receive(&mut self) -> Result<ReceiveResult, &'static str> {
        let packet = self.backend.as_mut().unwrap().receive()?;
        if let ReceiveResult::Ready(Some(packet)) = &packet {
            self.observations
                .lock()
                .unwrap()
                .packets
                .push(CapturedPacket {
                    bytes: packet.bytes.clone(),
                    timestamp: packet.timestamp_100ns,
                    keyframe: packet.keyframe,
                });
        }
        Ok(packet)
    }

    fn drain(&mut self) -> Result<bool, &'static str> {
        let complete = self.backend.as_mut().unwrap().drain()?;
        self.observations.lock().unwrap().drained += 1;
        Ok(complete)
    }

    fn close(mut self: Box<Self>) -> Result<(), &'static str> {
        self.backend.take().unwrap().close()?;
        self.observations.lock().unwrap().closed += 1;
        Ok(())
    }
}

impl Drop for RecordingBackend {
    fn drop(&mut self) {
        if let Some(backend) = self.backend.take() {
            drop(backend);
            self.observations.lock().unwrap().closed += 1;
        }
    }
}
