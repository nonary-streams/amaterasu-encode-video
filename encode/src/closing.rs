//! Broker requests retain their identity while encoder workers retire.
use crate::{native::Shared, sessions::Sessions};
use amaterasu_module_sdk::{Command, ModuleResult, Response, RuntimeClient};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const MAX_CLOSE_WAITERS: usize = 128;
const MAX_CLOSE_TARGETS: usize = 128;

struct CloseWaiter {
    owner: String,
    session: u64,
    result: Option<Value>,
}

#[derive(Default)]
pub(crate) struct Closing {
    waiters: BTreeMap<u64, CloseWaiter>,
    // Retain accepted cleanup intent independently of its cancellable waiter.
    // Unvalidated targets can name arbitrary IDs while the native lock is busy,
    // so they have their own bound rather than relying on session admission.
    targets: BTreeMap<(String, u64), bool>,
    // Only failed Open delivery creates an orphan. Session admission bounds
    // these entries to the same eight live encoder sessions.
    orphans: BTreeMap<u64, String>,
}

impl Closing {
    pub fn admit(&mut self, request: u64, owner: &str, session: u64) -> Result<(), &'static str> {
        if self.waiters.len() >= MAX_CLOSE_WAITERS {
            return Err("encoder_close_limit");
        }
        let target = (owner.to_owned(), session);
        if !self.targets.contains_key(&target) && self.targets.len() >= MAX_CLOSE_TARGETS {
            return Err("encoder_close_limit");
        }
        self.targets.entry(target).or_insert(false);
        self.waiters.entry(request).or_insert_with(|| CloseWaiter {
            owner: owner.into(),
            session,
            result: None,
        });
        Ok(())
    }

    pub fn rollback_open(&mut self, owner: &str, session: u64) {
        self.orphans.insert(session, owner.into());
    }

    pub fn is_empty(&self) -> bool {
        self.waiters.is_empty() && self.targets.is_empty() && self.orphans.is_empty()
    }

    pub fn advance(&mut self, runtime: &RuntimeClient, shared: &Shared) -> ModuleResult<()> {
        match shared.try_lock() {
            Ok(mut sessions) => self.poll_sessions(&mut sessions),
            Err(std::sync::TryLockError::WouldBlock) => (),
            Err(std::sync::TryLockError::Poisoned(_)) => return Err("encoder state failed".into()),
        }
        let mut failed = None;
        self.waiters.retain(|request, waiter| {
            match runtime.send(Command::CheckRequest { request: *request }) {
                Ok(Response::Ok) => (),
                Ok(response) if request_ended(&response) => return false,
                response => {
                    failed.get_or_insert_with(|| {
                        format!("encoder close request check failed: {response:?}")
                    });
                    return true;
                }
            }
            let Some(payload) = &waiter.result else {
                return true;
            };
            match runtime.send(Command::Complete {
                request: *request,
                payload: payload.clone(),
            }) {
                Ok(Response::Ok) => false,
                Ok(response) if request_ended(&response) => false,
                response => {
                    failed.get_or_insert_with(|| {
                        format!("encoder close completion failed: {response:?}")
                    });
                    true
                }
            }
        });
        match failed {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn poll_sessions(&mut self, sessions: &mut Sessions) {
        for ((owner, session), started) in &mut self.targets {
            if !*started && sessions.begin_close(owner, *session).is_ok() {
                *started = true;
            }
        }
        self.orphans
            .retain(|session, owner| sessions.begin_close(owner, *session).is_ok());
        sessions.poll_closing();
        for waiter in self.waiters.values_mut() {
            if waiter.result.is_none()
                && let Some(result) = sessions.close_result(&waiter.owner, waiter.session)
            {
                waiter.result = Some(result.unwrap_or_else(error_reply));
            }
        }
        self.orphans
            .retain(|session, owner| sessions.close_result(owner, *session).is_none());
        self.targets
            .retain(|(owner, session), _| sessions.close_result(owner, *session).is_none());
        // Copy every exact owner's result before releasing bounded receipts.
        // Native cancellation may have completed these sessions between ticks.
        sessions.release_close_receipts();
    }
}

fn request_ended(response: &Response) -> bool {
    matches!(response, Response::Error { code, .. } if code == "request_ended" || code == "not_found")
}

fn error_reply(code: &'static str) -> Value {
    json!({"ok":false,"code":code})
}
