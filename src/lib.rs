//! Video encoding control and the native CPU-frame and DMA-BUF interface.
mod bitstream;
mod closing;
mod factory;
#[cfg_attr(not(any(test, amaterasu_x265_hevc)), allow(dead_code))]
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
    Command, LogLevel, LogQueue, ManagedModule, ModuleResult, Response, RuntimeClient,
};
use model::Operation;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
const SERVICE: &str = "amaterasu.encode/1";
const TIMING_LOG_INTERVAL: Duration = Duration::from_secs(10);
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
    last_timing_log: Instant,
    shutting_down: bool,
    closing: closing::Closing,
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
            runtime.work_notifier(),
        )));
        let registration = Box::new(native::Registration::new(shared.clone()));
        runtime.require_success(Command::Register {
            service: SERVICE.into(),
        })?;
        // SAFETY: factory context/table remain stable until consumers release all
        // bindings; shared ownership pins session state independently of binding.
        unsafe {
            runtime.register_native_typed(
                SERVICE,
                amaterasu_module_sdk::media::native_contracts::ENCODER,
                &registration.factory,
            )?;
        }
        Ok(Self {
            runtime,
            shared,
            log_queue,
            last_timing_log: Instant::now(),
            shutting_down: false,
            closing: closing::Closing::default(),
            _registration: registration,
        })
    }
    fn tick(&mut self) -> ModuleResult<()> {
        self.log_queue.flush(&self.runtime);
        self.log_worker_timings();
        self.closing.advance(&self.runtime, &self.shared)?;
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
                    if let Ok(Operation::Close { session_id }) =
                        serde_json::from_value(payload.clone())
                    {
                        match self.closing.admit(id, &caller, session_id) {
                            Ok(()) => return self.closing.advance(&self.runtime, &self.shared),
                            Err(code) => Err(code),
                        }
                    } else {
                        self.control(&caller, payload)
                    }
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
                    self.closing.rollback_open(&caller, session);
                    self.closing.advance(&self.runtime, &self.shared)?;
                }
                Ok(())
            }
            _ => Err("encoder poll failed".into()),
        }
    }

    fn prepare_shutdown(&mut self, _remaining_ms: u64) -> ModuleResult<()> {
        self.shutting_down = true;
        Ok(())
    }

    fn quiesce(&mut self) -> ModuleResult<bool> {
        self.shutting_down = true;
        {
            match self.shared.try_lock() {
                Ok(mut sessions) => sessions.begin_shutdown(),
                Err(std::sync::TryLockError::WouldBlock) => return Ok(false),
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err("encoder state failed".into());
                }
            }
        }
        self.closing.advance(&self.runtime, &self.shared)?;
        match self.shared.try_lock() {
            Ok(sessions) => sessions
                .shutdown_status()
                .map(|ready| ready && self.closing.is_empty())
                .map_err(str::to_owned),
            Err(std::sync::TryLockError::WouldBlock) => Ok(false),
            Err(std::sync::TryLockError::Poisoned(_)) => Err("encoder state failed".into()),
        }
    }
}

impl EncodeModule {
    fn log_worker_timings(&mut self) {
        let now = Instant::now();
        if now.saturating_duration_since(self.last_timing_log) < TIMING_LOG_INTERVAL {
            return;
        }
        // Native clients share this lock. Skip a busy snapshot rather than
        // adding a wait to the host tick; retry on the next tick.
        let Ok(mut sessions) = self.shared.try_lock() else {
            return;
        };
        let summaries = sessions.timing_summaries(now);
        drop(sessions);
        self.last_timing_log = now;
        for summary in summaries {
            let _ = self.runtime.log(LogLevel::Info, "encoder.timing", &summary);
        }
    }

    fn control(&self, caller: &str, payload: Value) -> Result<(Value, Option<u64>), &'static str> {
        let operation: Operation =
            serde_json::from_value(payload).map_err(|_| "invalid_request")?;
        if self.shutting_down
            && matches!(&operation, Operation::Open { .. } | Operation::Probe { .. })
        {
            return Err("encoder_shutting_down");
        }
        if matches!(&operation, Operation::Close { .. }) {
            return Err("encoder_close_requires_request");
        }
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
            Operation::Close { .. } => unreachable!("close retains its broker request"),
            Operation::Status { session_id } => sessions.status(caller, session_id),
            Operation::Probe { .. } => unreachable!("probe runs outside the session lock"),
        }?;
        let created = (sessions.latest_id() != previous_id).then_some(sessions.latest_id());
        Ok((payload, created))
    }
}

impl Drop for EncodeModule {
    fn drop(&mut self) {
        // A panic in a native callback may poison this lock. Teardown still
        // owns every session, so recover the guard and join/retire its backend
        // workers before the module releases its final reference.
        let mut sessions = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = sessions.clear();
        self.log_queue.flush(&self.runtime);
    }
}

amaterasu_module_sdk::export_module!(EncodeModule);

#[cfg(test)]
mod module_tests;
