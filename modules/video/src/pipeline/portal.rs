//! Owner-scoped management-portal selection forwarded through the signed
//! PipeWire service grant. Portal node identifiers never leave this module.
use crate::{
    model::{PortalSourceType, error},
    rpc::{self, Send},
};
use amaterasu_module_sdk::{Command, Response};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const PIPEWIRE: &str = "amaterasu.capture-pipewire/1";
const SELECTED_SOURCE_SENTINEL: &str = "portal-selected-session-source";
const MAX_SELECTIONS: usize = 32;
const MAX_PENDING: usize = 32;

#[derive(Clone)]
struct Selection {
    source_type: PortalSourceType,
    provider_operation: String,
    output: Option<String>,
}

struct Pending {
    parent: u64,
    owner: String,
    operation_id: String,
    action: Action,
}

#[derive(Clone, Copy)]
enum Action {
    Select,
    Poll,
    Cancel,
}

#[derive(Default)]
pub(super) struct Control {
    selections: BTreeMap<(String, String), Selection>,
    current: Option<(String, String)>,
    pending: BTreeMap<u64, Pending>,
}

impl Control {
    pub(super) fn select(
        &mut self,
        owner: &str,
        request: u64,
        source_type: PortalSourceType,
        operation_id: String,
        enabled: bool,
        send: &mut Send<'_>,
    ) {
        if !enabled {
            rpc::complete(send, request, error("capture_provider_not_allowed"));
            return;
        }
        if !valid_operation_id(&operation_id) {
            rpc::complete(send, request, error("invalid_operation_id"));
            return;
        }
        let key = (owner.to_owned(), operation_id.clone());
        let action = match self.selections.get(&key) {
            Some(selection) if selection.source_type != source_type => {
                rpc::complete(send, request, error("operation_conflict"));
                return;
            }
            Some(_) => Action::Poll,
            None => {
                if self.selections.len() >= MAX_SELECTIONS {
                    rpc::complete(send, request, error("selection_limit"));
                    return;
                }
                let Ok(provider_operation) = new_operation_id() else {
                    rpc::complete(send, request, error("entropy_error"));
                    return;
                };
                self.current = None;
                for ((selection_owner, _), selection) in &mut self.selections {
                    if selection_owner.as_str() == owner {
                        selection.output = None;
                    }
                }
                self.selections.insert(
                    key.clone(),
                    Selection {
                        source_type,
                        provider_operation,
                        output: None,
                    },
                );
                Action::Select
            }
        };
        self.dispatch(owner, request, &operation_id, action, send);
    }

    pub(super) fn poll(
        &mut self,
        owner: &str,
        request: u64,
        operation_id: String,
        enabled: bool,
        send: &mut Send<'_>,
    ) {
        if !enabled {
            rpc::complete(send, request, error("capture_provider_not_allowed"));
            return;
        }
        if !valid_operation_id(&operation_id) {
            rpc::complete(send, request, error("invalid_operation_id"));
            return;
        }
        if !self
            .selections
            .contains_key(&(owner.to_owned(), operation_id.clone()))
        {
            rpc::complete(send, request, error("not_found"));
            return;
        }
        self.dispatch(owner, request, &operation_id, Action::Poll, send);
    }

    pub(super) fn cancel(
        &mut self,
        owner: &str,
        request: u64,
        operation_id: String,
        enabled: bool,
        send: &mut Send<'_>,
    ) {
        if !enabled {
            rpc::complete(send, request, error("capture_provider_not_allowed"));
            return;
        }
        if !valid_operation_id(&operation_id) {
            rpc::complete(send, request, error("invalid_operation_id"));
            return;
        }
        let key = (owner.to_owned(), operation_id.clone());
        let Some(selection) = self.selections.get_mut(&key) else {
            rpc::complete(send, request, error("not_found"));
            return;
        };
        selection.output = None;
        if self.current.as_ref() == Some(&key) {
            self.current = None;
        }
        self.dispatch(owner, request, &operation_id, Action::Cancel, send);
    }

    pub(super) fn resolve_output(&self, requested: &str) -> Option<String> {
        let (owner, operation_id) = self.current.as_ref()?;
        let selected = self
            .selections
            .get(&(owner.clone(), operation_id.clone()))?
            .output
            .as_deref()?;
        if requested == SELECTED_SOURCE_SENTINEL || requested == selected {
            Some(selected.to_owned())
        } else {
            None
        }
    }

    pub(super) fn tick(&mut self, send: &mut Send<'_>) {
        let Some(child) = self.pending.keys().next().copied() else {
            return;
        };
        let Some(pending) = self.pending.get(&child) else {
            return;
        };
        if !matches!(
            send(Command::CheckRequest {
                request: pending.parent
            }),
            Ok(Response::Ok)
        ) {
            let pending = self
                .pending
                .remove(&child)
                .expect("pending portal request exists");
            let _ = rpc::cancel(send, child);
            if matches!(pending.action, Action::Select) {
                self.selections
                    .remove(&(pending.owner, pending.operation_id));
            }
            return;
        }
        let Some(result) = rpc::result(send, child) else {
            return;
        };
        let pending = self
            .pending
            .remove(&child)
            .expect("pending portal request exists");
        let payload = match result {
            Ok(payload) => self.record_result(&pending, payload),
            Err(_) => error("portal_provider_failed"),
        };
        rpc::complete(send, pending.parent, payload);
    }

    fn dispatch(
        &mut self,
        owner: &str,
        request: u64,
        operation_id: &str,
        action: Action,
        send: &mut Send<'_>,
    ) {
        if self.pending.len() >= MAX_PENDING {
            rpc::complete(send, request, error("portal_request_limit"));
            return;
        }
        let Some(selection) = self
            .selections
            .get(&(owner.to_owned(), operation_id.to_owned()))
        else {
            rpc::complete(send, request, error("not_found"));
            return;
        };
        let action_name = match action {
            Action::Select => "select_source",
            Action::Poll => "poll_selection",
            Action::Cancel => "cancel_selection",
        };
        let mut payload = json!({
            "action": action_name,
            "operation_id": selection.provider_operation,
        });
        if matches!(action, Action::Select) {
            payload["source_type"] = json!(selection.source_type);
        }
        let runtime_operation = format!("video-portal-{request}-{action_name}");
        match send(Command::Request {
            service: PIPEWIRE.into(),
            payload,
            timeout_ms: 5000,
            operation_id: runtime_operation,
            parent: Some(request),
        }) {
            Ok(Response::Accepted { id }) => {
                self.pending.insert(
                    id,
                    Pending {
                        parent: request,
                        owner: owner.to_owned(),
                        operation_id: operation_id.to_owned(),
                        action,
                    },
                );
            }
            Ok(Response::Error { code, .. }) => {
                rpc::complete(send, request, error(&code));
            }
            _ => {
                rpc::complete(send, request, error("portal_provider_failed"));
            }
        }
    }

    fn record_result(&mut self, pending: &Pending, mut payload: Value) -> Value {
        if payload.get("ok").and_then(Value::as_bool) != Some(true) {
            return json!({"ok":false,"code":provider_error(&payload)});
        }
        let key = (pending.owner.clone(), pending.operation_id.clone());
        payload["operation_id"] = json!(pending.operation_id);
        if let Some(selection) = self.selections.get_mut(&key) {
            let selection_ready =
                payload.get("selection_state").and_then(Value::as_str) == Some("ready");
            let output = payload.get_mut("output").and_then(Value::as_object_mut);
            if selection_ready {
                selection.output = None;
                if let Some(output) = output {
                    output.remove("pipewire_node");
                    let valid_id = output
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| {
                            !id.is_empty() && id.len() <= 512 && !id.chars().any(char::is_control)
                        })
                        .map(str::to_owned);
                    selection.output = valid_id;
                    if selection.output.is_some() {
                        self.current = Some(key.clone());
                    }
                }
            } else {
                selection.output = None;
                if self.current.as_ref() == Some(&key) {
                    self.current = None;
                }
            }
        }
        payload
    }
}

fn provider_error(payload: &Value) -> &'static str {
    payload
        .get("error")
        .and_then(|value| value.get("code"))
        .and_then(Value::as_str)
        .filter(|code| {
            code.bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        })
        .and_then(|code| match code {
            "selection_permission_denied" => Some("portal_selection_denied"),
            "source_selection_denied" | "source_selection_cancelled" => {
                Some("portal_selection_cancelled")
            }
            "portal_unavailable" | "pipewire_unavailable" => Some("portal_unavailable"),
            "source_selection_busy" => Some("portal_selection_busy"),
            _ => None,
        })
        .unwrap_or("portal_selection_failed")
}

fn valid_operation_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn new_operation_id() -> Result<String, &'static str> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| "entropy_error")?;
    let mut value = String::from("video-");
    for byte in bytes {
        use std::fmt::Write;
        write!(&mut value, "{byte:02x}").map_err(|_| "entropy_error")?;
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use amaterasu_module_sdk::ModuleResult;

    #[test]
    fn selection_is_explicit_owner_scoped_and_hides_pipewire_node() {
        let mut control = Control::default();
        let mut commands = Vec::new();
        let mut send = |command| -> ModuleResult<Response> {
            commands.push(command);
            Ok(Response::Accepted { id: 41 })
        };
        control.select(
            "amaterasu.management",
            7,
            PortalSourceType::Monitor,
            "pick-1".into(),
            true,
            &mut send,
        );
        drop(send);
        assert!(
            matches!(commands.first(), Some(Command::Request { service, payload, .. }) if service == PIPEWIRE && payload["action"] == "select_source")
        );
        let provider_operation = match commands.first().unwrap() {
            Command::Request { payload, .. } => {
                payload["operation_id"].as_str().unwrap().to_owned()
            }
            _ => unreachable!(),
        };
        let hidden_provider_operation = provider_operation.clone();
        let mut result = None;
        let mut send = |command| -> ModuleResult<Response> {
            match command {
                Command::CheckRequest { .. } => Ok(Response::Ok),
                Command::TakeResult { .. } => Ok(Response::Result {
                    payload: json!({
                        "ok":true,"selection_state":"ready","operation_id":provider_operation,
                        "output":{"id":"portal-opaque","source_type":"monitor","width":1280,"height":720,"format":"bgra8","pipewire_node":77,"geometry_supported":false}
                    }),
                }),
                Command::Complete { payload, .. } => {
                    result = Some(payload);
                    Ok(Response::Ok)
                }
                _ => Ok(Response::Ok),
            }
        };
        control.tick(&mut send);
        assert_eq!(
            control.resolve_output(SELECTED_SOURCE_SENTINEL).as_deref(),
            Some("portal-opaque")
        );
        assert_eq!(
            control.resolve_output("portal-opaque").as_deref(),
            Some("portal-opaque")
        );
        let result = result.unwrap();
        assert_eq!(result["operation_id"], "pick-1");
        assert_ne!(result["operation_id"], hidden_provider_operation);
        assert_eq!(result["output"].get("pipewire_node"), None);
    }
}
