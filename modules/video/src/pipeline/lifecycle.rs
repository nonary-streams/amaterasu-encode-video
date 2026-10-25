//! Synchronous teardown cannot wait for other modules to tick. Stop delivery and
//! queue only exact known closes; provider destruction owns final crash recovery.
use super::*;

impl Pipeline {
    pub fn shutdown(&mut self, send: &mut Send<'_>) {
        self.enabled = false;
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
            stream.encoder_service(),
            json!({"action":"close","session_id":session}),
            format!("{}-shutdown-encoder", stream.operation),
            None,
        );
    }
    if let Some(capture) = stream.capture {
        let _ = rpc::submit(
            send,
            stream.capture_service(),
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
