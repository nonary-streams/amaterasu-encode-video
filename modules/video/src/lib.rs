//! Managed capture-to-encode coordination with bounded native packet delivery.
mod model;
mod native;
mod observation;
mod pipeline;
mod plane;
mod queue;
mod rpc;

use amaterasu_module_sdk::{Command, ManagedModule, ModuleResult, Response, RuntimeClient};
use model::{Operation, Settings, error};
use pipeline::Pipeline;
use plane::{GeometryProgress, NativePlane, Plane, Progress, Pump};
use std::sync::{Arc, Mutex};

pub const SERVICE: &str = "amaterasu.video/1";

struct Disabled;

impl Plane for Disabled {
    fn verify_geometry(
        &mut self,
        _: model::CaptureProvider,
        _: u64,
        _: Option<amaterasu_media::CaptureGeometry>,
    ) -> Result<GeometryProgress, &'static str> {
        Err("disabled")
    }

    fn pump(&mut self, _: Pump<'_>) -> Result<Progress, &'static str> {
        Err("disabled")
    }
    fn drain(&mut self, _: u64) -> Result<bool, &'static str> {
        Err("disabled")
    }
}

pub struct Video {
    runtime: RuntimeClient,
    pipeline: Pipeline,
    plane: Box<dyn Plane>,
    _factory: Box<native::Factory>,
}

impl ManagedModule for Video {
    fn start(runtime: RuntimeClient) -> ModuleResult<Self> {
        let Response::Result { payload } = runtime.send(Command::GetSettings {})? else {
            return Err("video settings unavailable".into());
        };
        let settings: Settings =
            serde_json::from_value(payload).map_err(|_| "invalid video settings")?;
        settings.validate()?;
        let queues = Arc::new(Mutex::new(queue::Queues::default()));
        let plane: Box<dyn Plane> = if settings.enabled {
            Box::new(NativePlane::bind(
                &runtime,
                queues.clone(),
                &settings.capture_providers,
            )?)
        } else {
            Box::new(Disabled)
        };
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).map_err(|_| "video entropy unavailable")?;
        let pipeline = Pipeline::new(
            settings,
            format!("{:016x}", u64::from_le_bytes(random)),
            queues.clone(),
        );
        let factory = native::Factory::new(queues);
        runtime.require_success(Command::Register {
            service: SERVICE.into(),
        })?;
        // SAFETY: factory is boxed before registration and retained through module
        // lifetime. Signed dependency order stops consumers before destroying video.
        unsafe { runtime.register_native(SERVICE, &factory.api) }?;
        Ok(Self {
            runtime,
            pipeline,
            plane,
            _factory: factory,
        })
    }

    fn tick(&mut self) -> ModuleResult<()> {
        let runtime = &self.runtime;
        self.pipeline.tick(&mut *self.plane, &mut |command| {
            runtime.send_ephemeral(command)
        });
        self.pipeline
            .flush_observations(&mut |command| runtime.send(command));
        match runtime.send(Command::Poll {})? {
            Response::Pending => Ok(()),
            Response::Request {
                id,
                caller,
                payload,
                ..
            } => {
                if !matches!(
                    runtime.send(Command::CheckRequest { request: id })?,
                    Response::Ok
                ) {
                    return Ok(());
                }
                let mut send = |command| runtime.send_ephemeral(command);
                match serde_json::from_value::<Operation>(payload) {
                    Ok(operation) => self.pipeline.handle(&caller, id, operation, &mut send),
                    Err(_) => {
                        rpc::complete(&mut send, id, error("invalid_request"));
                    }
                }
                Ok(())
            }
            _ => Err("video request polling failed".into()),
        }
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        let runtime = &self.runtime;
        self.pipeline
            .shutdown(&mut |command| runtime.send_ephemeral(command));
        self.pipeline.stop_observations();
    }
}

amaterasu_module_sdk::export_module!(Video);
