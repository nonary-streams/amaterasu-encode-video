//! Managed capture-to-encode coordination with bounded native packet delivery.
mod image_fit;
mod model;
mod native;
mod pipeline;
mod plane;
mod queue;
mod rpc;
mod source_preflight;

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
    source_preflight: source_preflight::SourcePreflight,
}

impl ManagedModule for Video {
    fn start(runtime: RuntimeClient) -> ModuleResult<Self> {
        let Response::Result { payload } = runtime.send(Command::GetSettings {})? else {
            return Err("video settings unavailable".into());
        };
        let settings: Settings =
            serde_json::from_value(payload).map_err(|_| "invalid video settings")?;
        settings.validate()?;
        let source_preflight = source_preflight::SourcePreflight::new(&settings);
        let queues = Arc::new(Mutex::new(queue::Queues::default()));
        let plane: Box<dyn Plane> = if settings.enabled {
            Box::new(
                NativePlane::bind(
                    &runtime,
                    queues.clone(),
                    &settings.resolved_providers(),
                    &settings.external_encoder_providers,
                    &settings.source_encoder_providers,
                    &settings.external_encoder_consumers,
                    settings.cpu_processor_service.as_deref(),
                    settings.gpu_processor_service.as_deref(),
                )?
                .with_resource_processor(&runtime, settings.resource_processor_service.as_deref())?
                .with_source_preflight_routes(
                    &runtime,
                    &settings.resolved_source_preflight_routes(),
                )
                .with_required_frame_leases(settings.require_frame_lease)
                .with_gpu_processor_opt_in(settings.gpu_processor_opt_in),
            )
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
        unsafe { amaterasu_module_sdk::video::register(&runtime, SERVICE, &factory.api) }?;
        Ok(Self {
            runtime,
            pipeline,
            plane,
            _factory: factory,
            source_preflight,
        })
    }

    fn prepare_shutdown(&mut self, remaining_ms: u64) -> ModuleResult<()> {
        self.pipeline.prepare_shutdown(remaining_ms);
        Ok(())
    }

    fn quiesce(&mut self) -> ModuleResult<bool> {
        let runtime = &self.runtime;
        self.pipeline.quiesce(&mut *self.plane, &mut |command| {
            send_pipeline_command(runtime, command)
        })
    }

    fn tick(&mut self) -> ModuleResult<()> {
        let _priority = amaterasu_module_sdk::threading::ScopedThreadPriority::best_effort(
            amaterasu_module_sdk::threading::ThreadPriority::High,
        );
        let runtime = &self.runtime;
        self.pipeline.tick(&mut *self.plane, &mut |command| {
            send_pipeline_command(runtime, command)
        });
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
                let mut send = |command| send_pipeline_command(runtime, command);
                if !self.source_preflight.operation_allowed(&caller, &payload) {
                    rpc::complete(&mut send, id, error("source_preflight_only"));
                    return Ok(());
                }
                if payload["action"] == "preflight_source" {
                    if self.pipeline.admission_closed() {
                        rpc::complete(&mut send, id, error("shutting_down"));
                        return Ok(());
                    }
                    let response =
                        self.source_preflight
                            .handle(runtime, self.plane.as_mut(), id, payload);
                    rpc::complete(&mut send, id, response);
                    return Ok(());
                }
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
        self.pipeline.stop_native(&mut *self.plane);
        self.pipeline
            .shutdown(&mut |command| send_pipeline_command(runtime, command));
    }
}

// Pipeline state machines retain each observed result before separately
// releasing its broker record. The ephemeral helper releases internally and
// can hide that result when the release acknowledgement is lost.
fn send_pipeline_command(runtime: &RuntimeClient, command: Command) -> ModuleResult<Response> {
    runtime.send(command)
}

amaterasu_module_sdk::export_module!(Video);

#[cfg(test)]
mod lifecycle_tests;
