//! Admission history is never evicted: closed operation IDs cannot allocate again.
use super::*;

pub(super) struct Record {
    pub stream: u64,
    pub specification: OpenSpec,
}

fn valid_operation_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && value.bytes().all(|byte| byte.is_ascii_graphic())
}

impl Pipeline {
    /// Replays observe the original stream without adopting its parent request.
    /// A failed replay completion must therefore never clean up that stream.
    pub(super) fn admit_operation(
        &self,
        caller: &str,
        request: u64,
        specification: &OpenSpec,
        send: &mut Send<'_>,
    ) -> bool {
        let Some(operation) = specification.operation_id.as_deref() else {
            return true;
        };
        let reply = if !valid_operation_id(operation) {
            Some(error("invalid_operation_id"))
        } else if let Some(record) = self.operations.get(&(caller.into(), operation.into())) {
            Some(if record.specification != *specification {
                error("operation_conflict")
            } else {
                self.streams
                    .get(&record.stream)
                    .map(|stream| stream.response(record.stream))
                    .unwrap_or_else(|| error("stream_closed"))
            })
        } else if self.operations.len() >= 128 {
            Some(error("operation_limit"))
        } else {
            None
        };
        if let Some(reply) = reply {
            rpc::complete(send, request, reply);
            false
        } else {
            true
        }
    }

    pub(super) fn lookup(&self, caller: &str, request: u64, operation: &str, send: &mut Send<'_>) {
        let reply = if !valid_operation_id(operation) {
            error("invalid_operation_id")
        } else {
            self.operations
                .get(&(caller.into(), operation.into()))
                .and_then(|record| {
                    self.streams
                        .get(&record.stream)
                        .map(|stream| stream.response(record.stream))
                })
                .unwrap_or_else(|| error("not_found"))
        };
        rpc::complete(send, request, reply);
    }
}
