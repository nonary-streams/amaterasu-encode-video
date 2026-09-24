//! Hardware H.264 encoding control and native CPU-frame interface.
#[cfg(any(windows, test))]
mod bitstream;
mod layout;
mod model;
mod native;
mod platform;
mod sessions;
mod worker;
use amaterasu_module_sdk::{Command, ManagedModule, ModuleResult, Response, RuntimeClient};
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
    _registration: Box<native::Registration>,
}

impl ManagedModule for EncodeModule {
    fn start(runtime: RuntimeClient) -> ModuleResult<Self> {
        let Response::Result { payload } = runtime.send(Command::GetSettings {})? else {
            return Err("encoder settings unavailable".into());
        };
        let settings: Settings =
            serde_json::from_value(payload).map_err(|_| "invalid encoder settings")?;
        let shared = Arc::new(Mutex::new(sessions::Sessions::new(settings.enabled)));
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
            _registration: registration,
        })
    }
    fn tick(&mut self) -> ModuleResult<()> {
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
        let mut sessions = self.shared.lock().map_err(|_| "encoder_state_failed")?;
        let previous_id = sessions.latest_id();
        let payload = match operation {
            Operation::Enumerate => {
                let encoders = if sessions.enabled {
                    worker::enumerate()?
                } else {
                    Vec::new()
                };
                let diagnostic = platform::last_failure().map(|detail| {
                    json!({"stage":detail.stage,"hresult":format!("0x{:08x}", detail.hresult)})
                });
                let metrics = platform::metrics();
                Ok(
                    json!({"ok":true,"enabled":sessions.enabled,"encoders":encoders,"diagnostic":diagnostic,
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
    }
}

amaterasu_module_sdk::export_module!(EncodeModule);

#[cfg(test)]
mod module_tests;
