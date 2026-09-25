use amaterasu_module_sdk::{Command, ModuleResult, Response};
use serde_json::Value;
pub type Send<'a> = dyn FnMut(Command) -> ModuleResult<Response> + 'a;

pub fn submit(
    send: &mut Send<'_>,
    service: &str,
    payload: Value,
    operation: String,
    parent: Option<u64>,
) -> Result<u64, &'static str> {
    match send(Command::Request {
        service: service.into(),
        payload,
        timeout_ms: 5000,
        operation_id: operation,
        parent,
    }) {
        Ok(Response::Accepted { id }) => Ok(id),
        _ => Err("request_failed"),
    }
}

pub fn result(send: &mut Send<'_>, request: u64) -> Option<Result<Value, &'static str>> {
    match send(Command::TakeResult { request }) {
        Ok(Response::Pending) => None,
        Ok(Response::Result { payload }) => Some(Ok(payload)),
        _ => Some(Err("request_failed")),
    }
}

pub fn cancel(send: &mut Send<'_>, request: u64) -> bool {
    for command in [
        Command::Cancel { request },
        Command::ReleaseResult { request },
    ] {
        match send(command) {
            Ok(Response::Ok) => {}
            Ok(Response::Error { code, .. }) if code == "not_found" => {}
            _ => return false,
        }
    }
    true
}

pub enum Admission {
    Accepted(u64),
    Rejected(String),
    Uncertain,
}

pub fn deliver(send: &mut Send<'_>, command: Command) -> Admission {
    match send(command) {
        Ok(Response::Accepted { id }) => Admission::Accepted(id),
        Ok(Response::Error { code, .. }) => Admission::Rejected(code),
        _ => Admission::Uncertain,
    }
}

pub fn complete(send: &mut Send<'_>, request: u64, payload: Value) -> bool {
    matches!(
        send(Command::Complete { request, payload }),
        Ok(Response::Ok)
    )
}

pub fn missing(payload: &Value) -> bool {
    let code = payload
        .get("code")
        .or_else(|| payload.get("error").and_then(|value| value.get("code")))
        .and_then(Value::as_str);
    matches!(
        code,
        Some("not_found" | "session_not_found" | "session_closed")
    )
}

pub fn end_parent(send: &mut Send<'_>, request: u64, payload: Value) -> bool {
    match send(Command::Complete { request, payload }) {
        Ok(Response::Ok) => true,
        Ok(Response::Error { code, .. }) if code == "request_ended" || code == "not_found" => true,
        _ => false,
    }
}
