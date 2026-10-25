//! Graceful output owns one deadline from capture shutdown through EOS acceptance.
use super::*;

pub(super) struct Grace {
    deadline: Instant,
    drain_sent: bool,
}

impl Pipeline {
    pub(super) fn output_action(
        &mut self,
        caller: &str,
        request: u64,
        id: u64,
        drain: bool,
        send: &mut Send<'_>,
    ) {
        let Some(stream) = self
            .streams
            .get_mut(&id)
            .filter(|stream| stream.owner == caller)
        else {
            rpc::complete(send, request, error("not_found"));
            return;
        };
        if drain && matches!(stream.stage, Stage::StopCapture | Stage::Draining) {
            rpc::complete(send, request, stream.response(id));
            return;
        }
        if stream.stage != Stage::Running {
            rpc::complete(send, request, error("invalid_state"));
            return;
        }
        if drain {
            stream.last_pixels = None;
            stream.last_output_at = None;
            stream.last_repeat_attempt_at = None;
            stream.grace = Some(Grace {
                deadline: Instant::now() + self.drain_timeout,
                drain_sent: false,
            });
            stream.force_idr = false;
            stream.stage = Stage::StopCapture;
        } else {
            stream.force_idr = true;
        }
        rpc::complete(send, request, stream.response(id));
    }

    pub(super) fn remember_failure(&mut self, id: u64, stream: &Stream) {
        if stream
            .error
            .is_none_or(|code| matches!(code, "closed_by_caller" | "drained"))
        {
            return;
        }
        let mut response = stream.response(id);
        response["state"] = json!("failed");
        if self.recent.len() >= 4 {
            self.recent.pop_front();
        }
        self.recent.push_back((stream.owner.clone(), response));
    }
}

pub(super) fn tick(
    stream: &mut Stream,
    id: u64,
    queues: &Arc<Mutex<Queues>>,
    plane: &mut dyn Plane,
) {
    let eos = queues.lock().is_ok_and(|queues| queues.eos_accepted(id));
    if eos {
        stream.cleanup("drained");
        return;
    }
    let Some(grace) = stream.grace.as_mut() else {
        stream.cleanup("invalid_drain_state");
        return;
    };
    if Instant::now() >= grace.deadline {
        stream.cleanup("drain_timeout");
        return;
    }
    if stream.stage == Stage::StopCapture {
        return;
    }
    // Once the encoder's EOS is buffered, only consumer acceptance or timeout can
    // advance output. Do not require further calls into an already drained provider.
    if queues.lock().is_ok_and(|queues| queues.eos_queued(id)) {
        return;
    }
    if !grace.drain_sent {
        match plane.drain(stream.session.unwrap()) {
            Ok(accepted) => grace.drain_sent = accepted,
            Err(code) => {
                stream.cleanup(code);
                return;
            }
        }
    }
    let input = Pump {
        video: id,
        provider: stream.provider,
        frame_path: stream.frame_path,
        capture: 0,
        encoder: stream.session.unwrap(),
        configuration: &stream.configuration,
        capture_due: false,
        constant_rate: stream.constant_rate,
        repeat_due: false,
        repeat_frame: None,
        timestamp_us: 0,
        sequence: 0,
        force_idr: false,
    };
    if let Err(code) = plane.pump(input) {
        stream.cleanup(code);
    }
}

#[cfg(test)]
pub(super) fn expire(stream: &mut Stream) {
    stream.grace.as_mut().unwrap().deadline = Instant::now();
}
