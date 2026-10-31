//! Checked adaptation of a negotiated provider stream into the existing packet queue.
//!
//! Authorization, the lifetime of the binding, and session ownership are the
//! caller's responsibility. Calls on a session must be serialized. Native
//! providers are trusted code and must obey the synchronous callback contract.
use crate::queue::Queues;
use amaterasu_media::*;
use amaterasu_module_sdk::abi::{STATUS_INVALID_ARGUMENT, STATUS_OK, Status};
use std::{ffi::c_void, mem::size_of};

#[derive(Clone, Copy)]
struct AccessUnit {
    id: u64,
    submission: u64,
    timestamp: u64,
    picture_flags: u32,
}

/// Immutable negotiated identity. This adapter only accepts the formats that
/// the downstream packet contract defines; no custom framing is inferred.
pub struct ProviderOutput {
    session: u64,
    queue_stream: u64,
    source: u64,
    generation: u64,
    codec: EncoderFormatId,
    framing: EncoderFormatId,
    packet_codec: u32,
    current: Option<AccessUnit>,
    last: Option<AccessUnit>,
    eos: bool,
    poisoned: bool,
}

fn standard_id(id: EncoderFormatId, value: u64) -> bool {
    id.namespace_id == ENCODER_FORMAT_NAMESPACE_STANDARD
        && id.schema == ENCODER_FORMAT_SCHEMA_V1
        && id.value == value
}

fn same_id(a: EncoderFormatId, b: EncoderFormatId) -> bool {
    a.namespace_id == b.namespace_id && a.value == b.value && a.schema == b.schema
}

impl ProviderOutput {
    /// The caller must authenticate the binding and verify that the session and
    /// output queue belong to the same stream owner before constructing this.
    pub fn new(
        session: u64,
        queue_stream: u64,
        config: &EncoderSessionConfig,
    ) -> Result<Self, Status> {
        let packet_codec = if standard_id(config.codec, CODEC_H264 as u64)
            && standard_id(config.framing, ENCODER_FRAMING_ANNEX_B)
        {
            CODEC_H264
        } else if standard_id(config.codec, CODEC_HEVC as u64)
            && standard_id(config.framing, ENCODER_FRAMING_ANNEX_B)
        {
            CODEC_HEVC
        } else if standard_id(config.codec, CODEC_AV1 as u64)
            && standard_id(config.framing, ENCODER_FRAMING_AV1_OBU)
        {
            CODEC_AV1
        } else {
            return Err(STATUS_INVALID_ARGUMENT);
        };
        if session == 0 || queue_stream == 0 || !encoder_session_config_valid(config) {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        Ok(Self {
            session,
            queue_stream,
            source: config.source_id,
            generation: config.config_generation,
            codec: config.codec,
            framing: config.framing,
            packet_codec,
            current: None,
            last: None,
            eos: false,
            poisoned: false,
        })
    }

    /// Receive one provider chunk. A queue rejection is returned to the provider
    /// synchronously so that it retains the same chunk for retry.
    ///
    /// # Safety
    /// `api` and its context must belong to the live authenticated binding for
    /// this session, and remain live throughout this call. The provider must
    /// obey its ABI and supply readable borrowed descriptors and data.
    pub unsafe fn receive(&mut self, api: &EncoderProviderApi, queues: &mut Queues) -> Status {
        if self.poisoned {
            return STATUS_INVALID_ARGUMENT;
        }
        if self.eos {
            return MEDIA_CLOSED;
        }
        let mut call = ReceiveCall {
            output: self,
            queues,
            calls: 0,
            callback_status: None,
        };
        let sink = EncoderOutputSink {
            context: (&mut call as *mut ReceiveCall<'_>).cast(),
            write: receive_chunk,
        };
        // SAFETY: caller ensures the table and binding are live; sink borrows call only synchronously.
        let status = unsafe {
            (api.receive)(
                api.base.context,
                call.output.session,
                MAX_PACKET_BYTES,
                sink,
            )
        };
        let valid = match call.callback_status {
            Some(callback) => call.calls == 1 && status == callback,
            None => call.calls == 0 && status != STATUS_OK,
        };
        if !valid {
            call.output.poisoned = true;
            return STATUS_INVALID_ARGUMENT;
        }
        status
    }

    /// # Safety
    /// The table must be the live binding for the authenticated session.
    pub unsafe fn drain(&mut self, api: &EncoderProviderApi) -> Status {
        if self.poisoned {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: the caller guarantees the table, context and session lifetime.
        unsafe { (api.drain)(api.base.context, self.session) }
    }

    fn validate(
        &self,
        chunk: &EncoderOutputChunk,
    ) -> Result<(EncodedPacket, Option<AccessUnit>, bool), Status> {
        const FLAGS: u32 = ENCODER_CHUNK_ACCESS_UNIT_START
            | ENCODER_CHUNK_ACCESS_UNIT_END
            | ENCODER_CHUNK_KEYFRAME
            | ENCODER_CHUNK_END_OF_STREAM;
        if self.eos
            || chunk.size < size_of::<EncoderOutputChunk>() as u32
            || chunk.session_id != self.session
            || chunk.source_id != self.source
            || chunk.config_generation != self.generation
            || !same_id(chunk.codec, self.codec)
            || !same_id(chunk.framing, self.framing)
            || chunk.flags & !FLAGS != 0
            || !encoder_borrowed_bytes_valid(chunk.bytes, MAX_PACKET_BYTES)
        {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        let eos = chunk.flags & ENCODER_CHUNK_END_OF_STREAM != 0;
        if eos {
            if chunk.flags != ENCODER_CHUNK_END_OF_STREAM
                || chunk.bytes.len != 0
                || self.current.is_some()
            {
                return Err(STATUS_INVALID_ARGUMENT);
            }
            return Ok((
                EncodedPacket {
                    codec: self.packet_codec,
                    flags: FLAG_END_OF_STREAM,
                    data: chunk.bytes.ptr,
                    len: 0,
                    timestamp_us: chunk.timestamp_us,
                    sequence: chunk.access_unit_id,
                },
                None,
                true,
            ));
        }
        if chunk.bytes.len == 0 || chunk.access_unit_id == 0 {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        let start = chunk.flags & ENCODER_CHUNK_ACCESS_UNIT_START != 0;
        let end = chunk.flags & ENCODER_CHUNK_ACCESS_UNIT_END != 0;
        let unit = AccessUnit {
            id: chunk.access_unit_id,
            submission: chunk.submission_id,
            timestamp: chunk.timestamp_us,
            picture_flags: chunk.flags & ENCODER_CHUNK_KEYFRAME,
        };
        match self.current {
            Some(current)
                if start
                    || current.id != unit.id
                    || current.submission != unit.submission
                    || current.timestamp != unit.timestamp
                    || current.picture_flags != unit.picture_flags =>
            {
                return Err(STATUS_INVALID_ARGUMENT);
            }
            None if !start
                || self
                    .last
                    .is_some_and(|last| unit.id == last.id || unit.timestamp < last.timestamp) =>
            {
                return Err(STATUS_INVALID_ARGUMENT);
            }
            _ => {}
        }
        let flags = if unit.picture_flags != 0 {
            FLAG_KEYFRAME
        } else {
            0
        } | if end { FLAG_END_OF_ACCESS_UNIT } else { 0 };
        Ok((
            EncodedPacket {
                codec: self.packet_codec,
                flags,
                data: chunk.bytes.ptr,
                len: chunk.bytes.len,
                timestamp_us: chunk.timestamp_us,
                sequence: chunk.access_unit_id,
            },
            Some(unit),
            end,
        ))
    }
}

struct ReceiveCall<'a> {
    output: &'a mut ProviderOutput,
    queues: &'a mut Queues,
    calls: usize,
    callback_status: Option<Status>,
}

unsafe extern "C" fn receive_chunk(
    context: *mut c_void,
    chunk: *const EncoderOutputChunk,
) -> Status {
    if context.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: receive supplies this context for the duration of the synchronous callback.
    let call = unsafe { &mut *context.cast::<ReceiveCall<'_>>() };
    call.calls += 1;
    if call.calls != 1 || chunk.is_null() {
        call.callback_status = Some(STATUS_INVALID_ARGUMENT);
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: the provider guarantees the descriptor is readable for the callback.
    let chunk = unsafe { &*chunk };
    let status = match call.output.validate(chunk) {
        Err(status) => status,
        Ok((packet, unit, end)) => {
            // SAFETY: the provider retains packet bytes throughout this callback.
            let status = unsafe { call.queues.push(call.output.queue_stream, &packet) };
            if status == STATUS_OK {
                if let Some(unit) = unit {
                    if end {
                        call.output.current = None;
                        call.output.last = Some(unit);
                    } else {
                        call.output.current = Some(unit);
                    }
                } else {
                    call.output.eos = true;
                }
            }
            status
        }
    };
    call.callback_status = Some(status);
    status
}

#[cfg(test)]
mod tests;
