//! Bounded asynchronous reads of the selected provider's live settings.
use super::{Stage, Stream};
use crate::{model::LiveProcessingSettings, plane::Plane, rpc::Send};
use amaterasu_module_sdk::requests::{
    RequestOptions, live_provider_settings_command, validate_live_provider_settings,
};
use amaterasu_module_sdk::{Command, LogLevel, Response};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

const POLL_INTERVAL: Duration = Duration::from_secs(1);

struct Pending {
    stream: u64,
    reference: LiveProcessingSettings,
    command: Command,
    request: Option<u64>,
    retry_at: Instant,
    result: Option<Value>,
}

#[derive(Default)]
pub(super) struct Control {
    pending: Option<Pending>,
    sequence: u64,
}

impl Control {
    pub(super) fn quiesce(&mut self, send: &mut Send<'_>) -> bool {
        let Some(pending) = self.pending.as_mut() else {
            return true;
        };
        if pending.request.is_none() {
            match send(pending.command.clone()) {
                Ok(Response::Accepted { id }) => pending.request = Some(id),
                Ok(Response::Error { .. }) => {
                    self.pending = None;
                    return true;
                }
                _ => return false,
            }
        }
        let request = pending.request.expect("recovered live settings request");
        if !crate::rpc::cancel(send, request) {
            return false;
        }
        self.pending = None;
        true
    }

    pub(super) fn shutdown(&mut self, send: &mut Send<'_>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        // Retry the identical admission once if its reply was lost. The broker
        // deduplicates the operation ID; this read creates no remote resource.
        let request = pending.request.or_else(|| match send(pending.command) {
            Ok(Response::Accepted { id }) => Some(id),
            _ => None,
        });
        if let Some(request) = request {
            if pending.result.is_some() {
                let _ = send(Command::ReleaseResult { request });
            } else {
                let _ = crate::rpc::cancel(send, request);
            }
        }
    }

    pub(super) fn tick(
        &mut self,
        streams: &mut BTreeMap<u64, Stream>,
        epoch: &str,
        lifecycle_service: Option<&str>,
        plane: &mut dyn Plane,
        send: &mut Send<'_>,
    ) {
        if let Some(mut pending) = self.pending.take() {
            if pending.request.is_none() {
                if Instant::now() < pending.retry_at {
                    self.pending = Some(pending);
                    return;
                }
                match send(pending.command.clone()) {
                    Ok(Response::Accepted { id }) => pending.request = Some(id),
                    Ok(Response::Error { .. }) => return,
                    _ => {
                        pending.retry_at = Instant::now() + POLL_INTERVAL;
                        self.pending = Some(pending);
                        return;
                    }
                }
            }
            let request = pending.request.expect("accepted request");
            let active = streams.get(&pending.stream).is_some_and(|stream| {
                stream.stage == Stage::Running
                    && stream
                        .configuration
                        .gpu_processing
                        .as_ref()
                        .and_then(|selection| selection.live_settings.as_ref())
                        == Some(&pending.reference)
            });
            if !active {
                let retired = if pending.result.is_some() {
                    match send(Command::ReleaseResult { request }) {
                        Ok(Response::Ok) => true,
                        Ok(Response::Error { code, .. }) if code == "not_found" => true,
                        _ => false,
                    }
                } else {
                    crate::rpc::cancel(send, request)
                };
                if !retired {
                    self.pending = Some(pending);
                }
                return;
            }
            if pending.result.is_none() {
                match send(Command::TakeResult { request }) {
                    Ok(Response::Result { payload }) => pending.result = Some(payload),
                    Ok(Response::Error { .. }) => pending.result = Some(Value::Null),
                    _ => {
                        self.pending = Some(pending);
                        return;
                    }
                }
            }
            match send(Command::ReleaseResult { request }) {
                Ok(Response::Ok) => {}
                Ok(Response::Error { code, .. }) if code == "not_found" => {}
                _ => {
                    self.pending = Some(pending);
                    return;
                }
            }
            let payload = pending.result.expect("result was received");
            let Some(stream) = streams.get_mut(&pending.stream) else {
                return;
            };
            let Some(selection) = stream.configuration.gpu_processing.as_mut() else {
                return;
            };
            let Some(reference) = selection.live_settings.as_mut() else {
                return;
            };
            if let Ok(Some(update)) = validate_live_provider_settings(
                &payload,
                &reference.target,
                &reference.provider,
                &reference.revision,
            ) {
                match plane.update_gpu(pending.stream, &update.settings) {
                    Ok(()) => {
                        reference.revision = update.revision;
                        stream.live_settings_rejected_revision = None;
                    }
                    Err("gpu_processor_update_internal") => {
                        stream.cleanup("gpu_processor_update_internal")
                    }
                    Err(reason) => {
                        let revision = update.revision.as_str();
                        if stream.live_settings_rejected_revision.as_deref() != Some(revision) {
                            let _ = send(Command::Log {
                                level: LogLevel::Warning as u8,
                                target: "video.processing".into(),
                                message: json!({"event":"gpu_processor_update_rejected", "stream":pending.stream, "revision":revision, "reason":reason}).to_string(),
                            });
                            stream.live_settings_rejected_revision = Some(revision.to_owned());
                        }
                    }
                }
            }
            return;
        }
        let now = Instant::now();
        let candidate = streams.iter_mut().find_map(|(&id, stream)| {
            let selection = stream.configuration.gpu_processing.as_ref()?;
            let reference = selection.live_settings.as_ref()?;
            if !selection.enabled
                || stream.stage != Stage::Running
                || now < stream.live_settings_next_poll
            {
                return None;
            }
            stream.live_settings_next_poll = now + POLL_INTERVAL;
            Some((id, reference.clone()))
        });
        let Some((stream, reference)) = candidate else {
            return;
        };
        let Some(lifecycle_service) = lifecycle_service else {
            return;
        };
        self.sequence = self.sequence.wrapping_add(1);
        let operation_id = format!("video-live-settings-{epoch}-{}", self.sequence);
        let Ok(command) = live_provider_settings_command(
            RequestOptions {
                service: lifecycle_service,
                timeout_ms: 5000,
                operation_id: &operation_id,
                parent: None,
            },
            reference.target.clone(),
            &reference.provider,
        ) else {
            return;
        };
        self.pending = Some(Pending {
            stream,
            reference,
            command,
            request: None,
            retry_at: now,
            result: None,
        });
        self.tick(streams, epoch, Some(lifecycle_service), plane, send);
    }
}
