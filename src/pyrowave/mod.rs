//! Optional built-in implementation of the same encoder SDK used by external plugins.
#[cfg(feature = "pyrowave-gpu")]
mod gpu;
mod input;
mod library;
mod native;
mod session;
#[cfg(test)]
mod tests;

use amaterasu_module_sdk::{Command, ModuleResult, RuntimeClient};
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;

pub const CONTROL_SERVICE: &str = "amaterasu.encode.pyrowave/1";
pub const PROVIDER_SERVICE: &str = "amaterasu.encode.pyrowave-provider/1";
pub const CONSUMER_SERVICE: &str = "amaterasu.encode.pyrowave-consumer/1";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub library_path: PathBuf,
    #[serde(default = "gpu_default")]
    pub gpu_only: bool,
}

fn gpu_default() -> bool {
    true
}

pub struct Plugin {
    shared: session::Shared,
    _registration: Box<native::Registration>,
}
impl Plugin {
    pub fn register(runtime: &RuntimeClient, settings: Settings) -> ModuleResult<Self> {
        if !settings.library_path.is_absolute() {
            return Err("PyroWave requires a trusted absolute library path".into());
        }
        let selected = if settings.gpu_only {
            #[cfg(feature = "pyrowave-gpu")]
            {
                library::Runtime::load_gpu(&settings.library_path)?
            }
            #[cfg(not(feature = "pyrowave-gpu"))]
            {
                return Err("GPU-bound PyroWave requires the pyrowave-gpu build feature".into());
            }
        } else {
            library::Runtime::load(&settings.library_path)?
        };
        let shared = session::State::new(selected);
        let registration = Box::new(native::Registration::new(shared.clone()));
        for service in [CONTROL_SERVICE, PROVIDER_SERVICE, CONSUMER_SERVICE] {
            runtime.require_success(Command::Register {
                service: service.into(),
            })?;
        }
        // SAFETY: the module owns these stable factory contexts until consumers
        // retire. SDK adapters retain the authenticated binding principal.
        unsafe {
            runtime.register_native_typed(
                PROVIDER_SERVICE,
                amaterasu_module_sdk::media::native_contracts::ENCODER_PROVIDER,
                &registration.provider,
            )?;
            runtime.register_native_typed(
                CONSUMER_SERVICE,
                amaterasu_module_sdk::media::native_contracts::FRAME_RESOURCE_CONSUMER,
                &registration.consumer,
            )?;
        }
        Ok(Self {
            shared,
            _registration: registration,
        })
    }
    pub fn control(
        &self,
        owner: &str,
        payload: Value,
    ) -> Result<(Value, Option<u64>), &'static str> {
        self.shared
            .try_lock()
            .map_err(|_| "encoder_busy")?
            .control(owner, payload)
    }
    pub fn close(&self, owner: &str, id: u64) {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .close(owner, id);
    }
}
