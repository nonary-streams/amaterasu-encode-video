//! Video encoding control and the native CPU-frame and DMA-BUF interface.
mod bitstream;
mod factory;
#[cfg_attr(
    not(any(test, amaterasu_x265_hevc, amaterasu_nvenc_cuda)),
    allow(dead_code)
)]
mod hevc;
#[cfg_attr(not(amaterasu_nvenc_cuda), allow(dead_code))]
mod input;
mod layout;
mod model;
mod native;
mod platform;
mod probe;
mod sessions;
mod worker;
use amaterasu_module_sdk::{
    Command, LogQueue, ManagedModule, ModuleResult, Response, RuntimeClient,
};
use model::Operation;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
const SERVICE: &str = "amaterasu.encode/1";
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    #[serde(default)]
    enabled: bool,
}

struct EncodeModule {
    runtime: RuntimeClient,
    shared: native::Shared,
    log_queue: LogQueue,
    _registration: Box<native::Registration>,
}

impl ManagedModule for EncodeModule {
    fn start(runtime: RuntimeClient) -> ModuleResult<Self> {
        let Response::Result { payload } = runtime.send(Command::GetSettings {})? else {
            return Err("encoder settings unavailable".into());
        };
        let settings: Settings =
            serde_json::from_value(payload).map_err(|_| "invalid encoder settings")?;
        let (log_emitter, log_queue) = runtime.log_emitter();
        let shared = Arc::new(Mutex::new(sessions::Sessions::with_log_emitter(
            settings.enabled,
            log_emitter,
        )));
        let registration = Box::new(native::Registration::new(shared.clone()));
        runtime.require_success(Command::Register {
            service: SERVICE.into(),
        })?;
        // SAFETY: factory context/table remain stable until consumers release all
        // bindings; shared ownership pins session state independently of binding.
        unsafe {
            runtime.register_native(SERVICE, &registration.factory)?;
        }
        Ok(Self {
            runtime,
            shared,
            log_queue,
            _registration: registration,
        })
    }
    fn tick(&mut self) -> ModuleResult<()> {
        self.log_queue.flush(&self.runtime);
        match self.runtime.send(Command::Poll {})? {
            Response::Pending => Ok(()),
            Response::Request {
                id,
                caller,
                service,
                payload,
                ..
            } => {
                if !matches!(
                    self.runtime.send(Command::CheckRequest { request: id })?,
                    Response::Ok
                ) {
                    return Ok(());
                }
                let result = if service == SERVICE {
                    self.control(&caller, payload)
                } else {
                    Err("invalid_service")
                };
                let (payload, created) =
                    result.unwrap_or_else(|code| (json!({"ok":false,"code":code}), None));
                if !matches!(
                    self.runtime.send(Command::Complete {
                        request: id,
                        payload
                    }),
                    Ok(Response::Ok)
                ) && let Some(session) = created
                {
                    let _ = self
                        .shared
                        .lock()
                        .map(|mut state| state.close(&caller, session));
                }
                Ok(())
            }
            _ => Err("encoder poll failed".into()),
        }
    }
}

impl EncodeModule {
    fn control(&self, caller: &str, payload: Value) -> Result<(Value, Option<u64>), &'static str> {
        let operation: Operation =
            serde_json::from_value(payload).map_err(|_| "invalid_request")?;
        if let Operation::Probe {
            encoder,
            configuration,
        } = operation
        {
            let factory = {
                let sessions = self.shared.lock().map_err(|_| "encoder_state_failed")?;
                sessions.probe_factory(caller, &encoder, &configuration)?
            };
            return probe::run(&*factory, encoder, configuration).map(|reply| (reply, None));
        }
        let mut sessions = self.shared.lock().map_err(|_| "encoder_state_failed")?;
        let previous_id = sessions.latest_id();
        let payload = match operation {
            Operation::Enumerate => {
                let encoders = sessions.enumerate()?;
                let diagnostic = platform::last_failure().map(|detail| {
                    json!({"stage":detail.stage,"hresult":format!("0x{:08x}", detail.hresult)})
                });
                let metrics = platform::metrics();
                let software_unavailable = platform::software_availability().err();
                Ok(
                    json!({"ok":true,"enabled":sessions.enabled,"encoders":encoders,"diagnostic":diagnostic,
                        "software_encoder":{"available":software_unavailable.is_none(),"reason":software_unavailable},
                        "metrics":{"need_input_events":metrics.need_input_events,"have_output_events":metrics.have_output_events,
                        "other_events":metrics.other_events,"process_input_calls":metrics.process_input_calls,
                        "process_output_calls":metrics.process_output_calls}}),
                )
            }
            Operation::Open {
                packet_boundaries,
                encoder,
                configuration,
                operation_id,
            } => sessions.open(
                caller,
                encoder,
                configuration,
                operation_id,
                packet_boundaries,
            ),
            Operation::Lookup { operation_id } => sessions.lookup(caller, &operation_id),
            Operation::Close { session_id } => sessions.close(caller, session_id),
            Operation::Status { session_id } => sessions.status(caller, session_id),
            Operation::Probe { .. } => unreachable!("probe runs outside the session lock"),
        }?;
        let created = (sessions.latest_id() != previous_id).then_some(sessions.latest_id());
        Ok((payload, created))
    }
}

impl Drop for EncodeModule {
    fn drop(&mut self) {
        if let Ok(mut sessions) = self.shared.lock() {
            sessions.clear();
        }
        self.log_queue.flush(&self.runtime);
    }
}

amaterasu_module_sdk::export_module!(EncodeModule);

#[cfg(test)]
mod module_tests;
