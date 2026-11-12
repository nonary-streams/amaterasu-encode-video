//! Fetch a new observation from the owner-scoped capture lease on every request.
//! An observation is not proof of display ownership or an invalidation fence.
use super::{CaptureProvider, Stage, Stream, observed_capture};
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

const MAX_PENDING: usize = 32;
const TIMEOUT: Duration = Duration::from_secs(5);

struct Pending {
    parent: u64,
    owner: String,
    stream: u64,
    capture: u64,
    operation: String,
    output: String,
    command: Command,
    child: Option<u64>,
    result: Option<Value>,
    started: Instant,
    cancelling: bool,
}

#[derive(Default)]
pub(super) struct Control {
    pending: BTreeMap<u64, Pending>,
}

impl Control {
    pub(super) fn quiesce(&mut self, send: &mut Send<'_>) -> bool {
        self.pending.retain(|_, pending| {
            // A failed transport check is not evidence that this delivered
            // parent ended. Keep its ownership until completion is confirmed.
            if !rpc::end_parent(send, pending.parent, error("video_shutdown")) {
                return true;
            }
            if pending.child.is_none() {
                match send(pending.command.clone()) {
                    Ok(Response::Accepted { id }) => pending.child = Some(id),
                    Ok(Response::Error { code, .. })
                        if matches!(
                            code.as_str(),
                            "permission_denied"
                                | "invalid_request"
                                | "unavailable"
                                | "capacity_exhausted"
                        ) =>
                    {
                        return false;
                    }
                    _ => return true,
                }
            }
            !rpc::cancel(send, pending.child.expect("recovered observation child"))
        });
        self.pending.is_empty()
    }

    pub(super) fn begin(
        &mut self,
        owner: &str,
        parent: u64,
        stream_id: u64,
        epoch: &str,
        streams: &BTreeMap<u64, Stream>,
        send: &mut Send<'_>,
    ) {
        let Some(stream) = streams
            .get(&stream_id)
            .filter(|stream| stream.owner == owner)
        else {
            rpc::complete(send, parent, error("not_found"));
            return;
        };
        let Some(capture) = stream.capture.filter(|_| {
            stream.stage == Stage::Running
                && stream.provider == CaptureProvider::Pipewire
                && stream.capture_output.starts_with("kwin:")
                && stream.source_id.is_none()
        }) else {
            rpc::complete(send, parent, error("capture_observation_unavailable"));
            return;
        };
        if self.pending.len() >= MAX_PENDING || self.pending.contains_key(&parent) {
            rpc::complete(send, parent, error("capture_observation_limit"));
            return;
        }
        let operation = stream.capture_operation.clone();
        let command = Command::Request {
            service: stream.provider.service().into(),
            payload: json!({"action":"lookup", "operation_id":operation}),
            operation_id: format!("video-observe-{epoch}-{parent}"),
            timeout_ms: TIMEOUT.as_millis() as u64,
            parent: Some(parent),
        };
        let mut pending = Pending {
            parent,
            owner: owner.into(),
            stream: stream_id,
            capture,
            operation,
            output: stream.capture_output.clone(),
            command,
            child: None,
            result: None,
            started: Instant::now(),
            cancelling: false,
        };
        match send(pending.command.clone()) {
            Ok(Response::Accepted { id }) => pending.child = Some(id),
            Ok(Response::Error { .. }) => {
                rpc::complete(send, parent, error("capture_observation_unavailable"));
                return;
            }
            // An admission reply can be lost. Retry this exact broker operation,
            // including after parent termination, until its child ID is recovered.
            _ => {}
        }
        self.pending.insert(parent, pending);
    }

    pub(super) fn tick(&mut self, streams: &BTreeMap<u64, Stream>, send: &mut Send<'_>) {
        let parents: Vec<u64> = self.pending.keys().copied().collect();
        for parent in parents {
            let pending = self
                .pending
                .get_mut(&parent)
                .expect("selected pending request");
            if !pending.cancelling
                && (!matches!(
                    send(Command::CheckRequest { request: parent }),
                    Ok(Response::Ok)
                ) || pending.started.elapsed() >= TIMEOUT)
            {
                if pending.started.elapsed() >= TIMEOUT {
                    rpc::complete(send, parent, error("capture_observation_unavailable"));
                }
                pending.cancelling = true;
            }
            if pending.child.is_none() {
                match send(pending.command.clone()) {
                    Ok(Response::Accepted { id }) => pending.child = Some(id),
                    // The broker retries existing records before checking parent
                    // authority; explicit rejection establishes no child exists.
                    Ok(Response::Error { .. }) => {
                        if !pending.cancelling {
                            rpc::complete(send, parent, error("capture_observation_unavailable"));
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
                        rpc::complete(send, parent, error("capture_observation_unavailable"));
                        pending.cancelling = true;
                        continue;
                    }
                }
            }
            // TakeResult is non-destructive. Do not return a snapshot or forget
            // its child until the broker confirms release of its retained result.
            if !release(send, child) {
                continue;
            }
            let pending = self
                .pending
                .remove(&parent)
                .expect("selected pending request");
            let payload = pending
                .result
                .as_ref()
                .and_then(|payload| verified(streams, &pending, payload))
                .unwrap_or_else(|| error("capture_observation_unavailable"));
            rpc::complete(send, parent, payload);
        }
    }

    pub(super) fn shutdown(&mut self, send: &mut Send<'_>) {
        for (_, pending) in std::mem::take(&mut self.pending) {
            rpc::complete(
                send,
                pending.parent,
                error("capture_observation_unavailable"),
            );
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

fn release(send: &mut Send<'_>, child: u64) -> bool {
    match send(Command::ReleaseResult { request: child }) {
        Ok(Response::Ok) => true,
        Ok(Response::Error { code, .. }) if code == "not_found" => true,
        _ => false,
    }
}

fn verified(streams: &BTreeMap<u64, Stream>, pending: &Pending, payload: &Value) -> Option<Value> {
    let stream = streams.get(&pending.stream)?;
    if stream.owner != pending.owner
        || stream.stage != Stage::Running
        || stream.provider != CaptureProvider::Pipewire
        || stream.capture != Some(pending.capture)
        || stream.capture_operation != pending.operation
        || stream.capture_output != pending.output
        || payload["ok"] != true
        || payload["stream"].as_u64() != Some(pending.capture)
        || payload["operation_id"].as_str() != Some(pending.operation.as_str())
        || payload["output"].as_str() != Some(pending.output.as_str())
    {
        return None;
    }
    let observed = observed_capture(payload).ok()??;
    if pending.output.strip_prefix("kwin:") != Some(observed.output_name.as_str()) {
        return None;
    }
    Some(json!({"ok":true,"stream":pending.stream,"observed_capture":observed}))
}
