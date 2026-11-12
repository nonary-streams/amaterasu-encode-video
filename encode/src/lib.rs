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
mod preparation;
mod probe;
#[cfg(feature = "pyrowave")]
mod pyrowave;
mod sessions;
mod worker;
use amaterasu_module_sdk::{
    Command, LogQueue, ManagedModule, ModuleResult, Response, RuntimeClient,
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
    #[cfg(feature = "pyrowave")]
    #[serde(default)]
    pyrowave: Option<pyrowave::Settings>,
}

struct EncodeModule {
    runtime: RuntimeClient,
    shared: native::Shared,
    log_queue: LogQueue,
    last_timing_log: Instant,
    shutting_down: bool,
    closing: closing::Closing,
    preparation: preparation::Preparation,
    _registration: Box<native::Registration>,
    #[cfg(feature = "pyrowave")]
    pyrowave: Option<pyrowave::Plugin>,
}

impl ManagedModule for EncodeModule {
    fn start(runtime: RuntimeClient) -> ModuleResult<Self> {
        let Response::Result { payload } = runtime.send(Command::GetSettings {})? else {
            return Err("encoder settings unavailable".into());
        };
        let settings: Settings =
            serde_json::from_value(payload).map_err(|_| "invalid encoder settings")?;
        #[cfg(feature = "pyrowave")]
        let pyrowave = settings
            .pyrowave
            .filter(|_| settings.enabled)
            .map(|settings| pyrowave::Plugin::register(&runtime, settings))
            .transpose()?;
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
            preparation: preparation::Preparation::new(runtime.work_notifier())?,
            runtime,
            shared,
            log_queue,
            last_timing_log: Instant::now(),
            shutting_down: false,
            closing: closing::Closing::default(),
            _registration: registration,
            #[cfg(feature = "pyrowave")]
            pyrowave,
        })
    }
    fn tick(&mut self) -> ModuleResult<()> {
        self.log_queue.flush(&self.runtime);
        self.log_worker_timings();
        self.preparation.advance(
            &self.runtime,
            &self.shared,
            &mut self.closing,
            self.shutting_down,
        )?;
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
                #[cfg(feature = "pyrowave")]
                let pyrowave_request = service == pyrowave::CONTROL_SERVICE;
                let result = if service == SERVICE {
                    let operation = serde_json::from_value::<Operation>(payload.clone());
                    let needs_preparation = match &operation {
                        Ok(
                            Operation::Enumerate | Operation::Probe { .. } | Operation::Open { .. },
                        ) => true,
                        Ok(Operation::Lookup { operation_id }) => {
                            self.preparation.has_pending_open(&caller, operation_id)
                        }
                        _ => false,
                    };
                    if needs_preparation {
                        if self.shutting_down {
                            Err("encoder_shutting_down")
                        } else {
                            match self
                                .preparation
                                .admit(id, caller.clone(), operation.unwrap())
                            {
                                Ok(()) => {
                                    return self.preparation.advance(
                                        &self.runtime,
                                        &self.shared,
                                        &mut self.closing,
                                        false,
                                    );
                                }
                                Err(code) => Err(code),
                            }
                        }
                    } else if let Ok(Operation::Close { session_id }) = operation {
                        match self.closing.admit(id, &caller, session_id) {
                            Ok(()) => return self.closing.advance(&self.runtime, &self.shared),
                            Err(code) => Err(code),
                        }
                    } else {
                        self.control(&caller, payload)
                    }
                } else {
                    #[cfg(feature = "pyrowave")]
                    if pyrowave_request {
                        if self.shutting_down
                            && matches!(payload["action"].as_str(), Some("open" | "probe"))
                        {
                            Err("encoder_shutting_down")
                        } else {
                            self.pyrowave
                                .as_ref()
                                .ok_or("pyrowave_disabled")
                                .and_then(|plugin| plugin.control(&caller, payload))
                        }
                    } else {
                        Err("invalid_service")
                    }
                    #[cfg(not(feature = "pyrowave"))]
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
                    #[cfg(feature = "pyrowave")]
                    if pyrowave_request {
                        if let Some(plugin) = &self.pyrowave {
                            plugin.close(&caller, session);
                        }
                        return Ok(());
                    }
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
        self.preparation
            .advance(&self.runtime, &self.shared, &mut self.closing, true)?;
        self.closing.advance(&self.runtime, &self.shared)?;
        match self.shared.try_lock() {
            Ok(sessions) => sessions
                .shutdown_status()
                .map(|ready| ready && self.closing.is_empty() && self.preparation.retired())
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
            amaterasu_module_sdk::diagnostics::log_window(
                &self.runtime,
                "encoder.timing",
                &summary,
            );
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
        let mut sessions = self.shared.try_lock().map_err(|_| "encoder_state_busy")?;
        let payload = match operation {
            Operation::Enumerate | Operation::Probe { .. } | Operation::Open { .. } => {
                return Err("encoder_preparation_requires_request");
            }
            Operation::Lookup { operation_id } => sessions.lookup(caller, &operation_id),
            Operation::Close { .. } => unreachable!("close retains its broker request"),
            Operation::Status { session_id } => sessions.status(caller, session_id),
        }?;
        Ok((payload, None))
    }
}

impl Drop for EncodeModule {
    fn drop(&mut self) {
        self.preparation.join();
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

#[cfg(not(feature = "pyrowave"))]
amaterasu_module_sdk::export_module!(EncodeModule);
// PyroWave control operations can create Vulkan devices or encode a probe.
// Native factory callbacks own separate Arc-backed state and never borrow this
// module instance, so its complete control lifecycle can run on the SDK worker.
#[cfg(feature = "pyrowave")]
amaterasu_module_sdk::export_module!(
    amaterasu_module_sdk::service_worker::ServiceWorker<EncodeModule>
);

#[cfg(test)]
mod module_tests;
