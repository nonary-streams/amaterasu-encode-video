//! Cooperative retirement retains exact cleanup ownership while providers tick.
//! Synchronous destruction only queues known closes as a final containment path.
use super::*;

impl Pipeline {
    pub fn admission_closed(&self) -> bool {
        self.shutdown_deadline.is_some() || self.quiescing
    }

    pub fn prepare_shutdown(&mut self, remaining_ms: u64) {
        let now = Instant::now();
        let deadline = now
            .checked_add(Duration::from_millis(remaining_ms))
            .unwrap_or(now);
        self.shutdown_deadline = Some(
            self.shutdown_deadline
                .map_or(deadline, |previous| previous.min(deadline)),
        );
    }

    /// Retire native borrows before queuing exact control cleanup. Providers
    /// remain tickable until each retained request and resource has settled.
    pub fn quiesce(
        &mut self,
        plane: &mut dyn Plane,
        send: &mut Send<'_>,
    ) -> amaterasu_module_sdk::ModuleResult<bool> {
        if !self.quiescing {
            self.quiescing = true;
            if self.shutdown_deadline.is_none() {
                self.prepare_shutdown(20_000);
            }
            for (&id, stream) in &mut self.streams {
                plane.finish_stream(id);
                if !matches!(
                    stream.stage,
                    Stage::FindEncoder
                        | Stage::CloseEncoder
                        | Stage::FindCapture
                        | Stage::CloseCapture
                        | Stage::Failed
                ) {
                    stream.cleanup("video_shutdown");
                }
            }
        }
        let queues_closed = self.queues.try_lock().is_ok_and(|mut queues| {
            queues.clear();
            true
        });
        if queues_closed {
            self.pending_queue_closes.clear();
        }
        let controls_closed = self.portal.quiesce(send)
            & self.observations.quiesce(send)
            & self.live_settings.quiesce(send);
        let ready = queues_closed && controls_closed && self.streams.is_empty();
        if !ready
            && self
                .shutdown_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err("video shutdown deadline elapsed with cleanup pending".into());
        }
        Ok(ready)
    }

    pub fn shutdown(&mut self, send: &mut Send<'_>) {
        self.enabled = false;
        self.observations.shutdown(send);
        self.live_settings.shutdown(send);
        if let Ok(mut queues) = self.queues.lock() {
            queues.clear();
        }
        for stream in self.streams.values_mut() {
            if let Some(request) = stream.pending.take() {
                stream.unsettled = Some(request);
            }
            if let Some(parent) = stream.parent.take() {
                stream.ending_parent = Some(parent);
                stream.error = Some("video_shutdown");
            }
            if settle(stream, send) {
                close_known(stream, send);
            }
        }
        self.streams.clear();
    }
}

fn close_known(stream: &Stream, send: &mut Send<'_>) {
    if let Some(session) = stream.session {
        let _ = rpc::submit(
            send,
            &stream.encoder_service,
            json!({"action":"close","session_id":session}),
            format!("{}-shutdown-encoder", stream.operation),
            None,
        );
    }
    if let Some(capture) = stream.capture {
        let _ = rpc::submit(
            send,
            stream.provider.service(),
            json!({"action":"close","stream":capture}),
            format!("{}-shutdown-capture", stream.operation),
            None,
        );
    }
}

/// Lookup must not run while an older request can still create a resource later.
pub(super) fn settle(stream: &mut Stream, send: &mut Send<'_>) -> bool {
    if let Some(request) = stream.unsettled {
        if !rpc::cancel(send, request) {
            return unsettled(stream);
        }
        stream.unsettled = None;
    }
    if let Some(parent) = stream.ending_parent {
        if !rpc::end_parent(send, parent, error(stream.error.unwrap_or("request_ended"))) {
            return unsettled(stream);
        }
        stream.ending_parent = None;
    }
    if let Some(command) = stream.uncertain.clone() {
        // Open's parent is confirmed terminal before this exact retry. An expired
        // broker dedup record therefore cannot authorize a fresh child open.
        // Parentless cleanup retries are only idempotent lookup/exact-ID close.
        match rpc::deliver(send, command) {
            rpc::Admission::Accepted(request) => {
                stream.unsettled = Some(request);
                stream.uncertain = None;
            }
            rpc::Admission::Rejected(_) => stream.uncertain = None,
            rpc::Admission::Uncertain => return unsettled(stream),
        }
        if let Some(request) = stream.unsettled {
            if !rpc::cancel(send, request) {
                return unsettled(stream);
            }
            stream.unsettled = None;
        }
    }
    stream.settlement_attempts = 0;
    true
}

fn unsettled(stream: &mut Stream) -> bool {
    stream.settlement_attempts += 1;
    if stream.settlement_attempts >= 3 {
        stream.stage = Stage::Failed;
        stream.error = Some("request_cancellation_unconfirmed");
    }
    false
}
