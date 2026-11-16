//! Correlated read-only probe of the selected encoder and native source path.
use super::{Configuration, EncoderIdentity, Plane};
use crate::{
    model::error,
    rpc::{self, Send},
};
use amaterasu_module_sdk::{Command, Response};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

const MAX_PENDING: usize = 8;
const TIMEOUT: Duration = Duration::from_secs(5);

struct Pending {
    parent: u64,
    service: String,
    source_id: u64,
    submitted: bool,
    encoder: EncoderIdentity,
    configuration: Configuration,
    command: Command,
    child: Option<u64>,
    result: Option<Value>,
    started: Instant,
    cancelling: bool,
}

pub(super) struct Probe {
    pub source_id: u64,
    pub submitted: bool,
    pub encoder: EncoderIdentity,
    pub configuration: Configuration,
    pub service: String,
}

#[derive(Default)]
pub(super) struct Control {
    pending: BTreeMap<u64, Pending>,
}

impl Control {
    pub(super) fn begin(&mut self, parent: u64, probe: Probe, epoch: &str, send: &mut Send<'_>) {
        if self.pending.len() >= MAX_PENDING || self.pending.contains_key(&parent) {
            rpc::complete(send, parent, error("encoder_probe_queue_full"));
            return;
        }
        let Probe {
            service,
            source_id,
            submitted,
            encoder,
            configuration,
        } = probe;
        let command = Command::Request {
            service: service.clone(),
            payload: json!({"action":"probe_source", "source_id":source_id, "encoder":encoder, "configuration":configuration}),
            operation_id: format!("video-source-probe-{epoch}-{parent}"),
            timeout_ms: TIMEOUT.as_millis() as u64,
            parent: Some(parent),
        };
        let mut pending = Pending {
            parent,
            service,
            source_id,
            submitted,
            encoder,
            configuration,
            command,
            child: None,
            result: None,
            started: Instant::now(),
            cancelling: false,
        };
        match send(pending.command.clone()) {
            Ok(Response::Accepted { id }) => pending.child = Some(id),
            Ok(Response::Error { .. }) => {
                rpc::complete(send, parent, error("encoder_probe_unavailable"));
                return;
            }
            _ => {} // Retry the identical operation if admission was ambiguous.
        }
        self.pending.insert(parent, pending);
    }

    pub(super) fn tick(&mut self, plane: &mut dyn Plane, send: &mut Send<'_>) {
        for parent in self.pending.keys().copied().collect::<Vec<_>>() {
            let pending = self.pending.get_mut(&parent).expect("selected probe");
            if !pending.cancelling
                && (!matches!(
                    send(Command::CheckRequest { request: parent }),
                    Ok(Response::Ok)
                ) || pending.started.elapsed() >= TIMEOUT)
            {
                if pending.started.elapsed() >= TIMEOUT {
                    rpc::complete(send, parent, error("encoder_probe_unavailable"));
                }
                pending.cancelling = true;
            }
            if pending.child.is_none() {
                match send(pending.command.clone()) {
                    Ok(Response::Accepted { id }) => pending.child = Some(id),
                    Ok(Response::Error { .. }) => {
                        if !pending.cancelling {
                            rpc::complete(send, parent, error("encoder_probe_unavailable"));
                        }
                        self.pending.remove(&parent);
                        continue;
                    }
                    _ => continue,
                }
            }
            let child = pending.child.expect("child recovered");
            if pending.cancelling {
                if rpc::cancel(send, child) {
                    self.pending.remove(&parent);
                }
                continue;
            }
            if pending.result.is_none() {
                match rpc::result(send, child) {
                    None => continue,
                    Some(Ok(payload)) => pending.result = Some(payload),
                    Some(Err(_)) => {
                        rpc::complete(send, parent, error("encoder_probe_unavailable"));
                        pending.cancelling = true;
                        continue;
                    }
                }
            }
            let released = match send(Command::ReleaseResult { request: child }) {
                Ok(Response::Ok) => true,
                Ok(Response::Error { code, .. }) if code == "not_found" => true,
                _ => false,
            };
            if !released {
                continue;
            }
            let pending = self.pending.remove(&parent).expect("selected probe");
            let reply = pending.result.as_ref().expect("result read");
            let correlated = reply["ok"] == true
                && reply["usable"] == true
                && reply["settled"] == true
                && reply["source_id"].as_u64() == Some(pending.source_id)
                && reply["encoder"] == json!(pending.encoder)
                && reply["configuration"] == json!(pending.configuration);
            let payload = if correlated {
                match if pending.submitted {
                    Ok(())
                } else {
                    plane.probe_source(&pending.service, pending.source_id, &pending.configuration)
                } {
                    Ok(()) => {
                        json!({"ok":true,"source_id":pending.source_id,"encoder":pending.encoder,"profile":pending.configuration.extension_profile})
                    }
                    Err(code) => error(code),
                }
            } else {
                error("encoder_probe_unavailable")
            };
            rpc::complete(send, parent, payload);
        }
    }

    pub(super) fn shutdown(&mut self, send: &mut Send<'_>) {
        for (_, pending) in std::mem::take(&mut self.pending) {
            rpc::complete(send, pending.parent, error("encoder_probe_unavailable"));
            let child = pending.child.or_else(|| match send(pending.command) {
                Ok(Response::Accepted { id }) => Some(id),
                _ => None,
            });
            if let Some(child) = child {
                let _ = rpc::cancel(send, child);
            }
        }
    }
}
