//! Moves finished encoder output into the stream's bounded packet queue.
//!
//! The consumer reads that queue later in the same host pass, so every packet
//! the selected encoder already finished is taken now. Leaving one behind would
//! delay it by a whole pass (and the encoder's next readiness hint).
use crate::queue::Queues;
use amaterasu_media::{
    EncodedPacket, EncoderApi, FLAG_END_OF_STREAM, MEDIA_BUSY, MEDIA_PENDING, PacketSink,
};
use amaterasu_module_sdk::abi::{STATUS_INVALID_ARGUMENT, STATUS_OK, Status};
use std::{
    ffi::c_void,
    sync::{Arc, Mutex},
    time::Instant,
};

/// Receive calls per pump. Equal to the stream queue's packet bound, so the
/// loop ends by backpressure or an empty encoder before exhausting it.
const RECEIVE_BUDGET: usize = 4;

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Received {
    pub encoder_poll_us: u64,
    /// Mean queue-admission time per delivered packet, so recording it
    /// `packets` times preserves the total.
    pub admission_us: u64,
    pub packets: u64,
}

struct Destination<'a> {
    queues: &'a Arc<Mutex<Queues>>,
    video: u64,
    admission_us_total: u64,
    deliveries: u64,
    accepted: u64,
    end_of_stream: bool,
}

unsafe extern "C" fn queue_packet(context: *mut c_void, packet: *const EncodedPacket) -> Status {
    if context.is_null() || packet.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: the encoder lends the packet and this destination only for the
    // synchronous receive call below.
    let destination = unsafe { &mut *context.cast::<Destination<'_>>() };
    let packet = unsafe { &*packet };
    let started = Instant::now();
    destination.deliveries = destination.deliveries.saturating_add(1);
    let status = if let Ok(mut queues) = destination.queues.try_lock() {
        // SAFETY: packet bytes remain borrowed until this callback returns.
        unsafe { queues.push(destination.video, packet) }
    } else {
        MEDIA_BUSY
    };
    destination.admission_us_total = destination
        .admission_us_total
        .saturating_add(super::elapsed_micros(started));
    if status == STATUS_OK {
        destination.accepted = destination.accepted.saturating_add(1);
        destination.end_of_stream |= packet.flags == FLAG_END_OF_STREAM;
    }
    status
}

/// Takes every packet the encoder has ready, in order, until it reports none,
/// the stream queue applies backpressure, or end of stream is queued (a
/// drained encoder must not be called again).
pub(super) fn receive(
    api: &EncoderApi,
    encoder: u64,
    video: u64,
    queues: &Arc<Mutex<Queues>>,
) -> Result<Received, &'static str> {
    let mut destination = Destination {
        queues,
        video,
        admission_us_total: 0,
        deliveries: 0,
        accepted: 0,
        end_of_stream: false,
    };
    let sink = PacketSink {
        context: (&mut destination as *mut Destination<'_>).cast(),
        write: queue_packet,
    };
    let started = Instant::now();
    for _ in 0..RECEIVE_BUDGET {
        let accepted_before = destination.accepted;
        // SAFETY: the stack sink and destination outlive this synchronous call;
        // the encoder makes at most one callback per receive.
        match unsafe { (api.receive)(api.base.context, encoder, sink) } {
            STATUS_OK => {}
            MEDIA_PENDING | MEDIA_BUSY => break,
            _ => return Err("encoder_failed"),
        }
        if destination.end_of_stream || destination.accepted == accepted_before {
            break;
        }
    }
    Ok(Received {
        encoder_poll_us: super::elapsed_micros(started),
        admission_us: destination
            .admission_us_total
            .checked_div(destination.deliveries)
            .unwrap_or(0),
        packets: destination.deliveries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use amaterasu_media::{CODEC_H264, FLAG_END_OF_ACCESS_UNIT};
    use amaterasu_module_sdk::abi::{AbiHeader, NativeInterface};
    use std::collections::VecDeque;

    /// A substitute encoder provider: any implementation of the v1 receive
    /// contract, independent of the built-in encode module.
    struct ScriptedEncoder {
        ready: VecDeque<u32>,
        calls: usize,
        delivered: u64,
    }

    impl ScriptedEncoder {
        fn new(ready: impl IntoIterator<Item = u32>) -> Self {
            Self {
                ready: ready.into_iter().collect(),
                calls: 0,
                delivered: 0,
            }
        }
    }

    unsafe extern "C" fn scripted_receive(
        context: *mut c_void,
        _: u64,
        sink: PacketSink,
    ) -> Status {
        // SAFETY: the test keeps the encoder alive for every receive call.
        let encoder = unsafe { &mut *context.cast::<ScriptedEncoder>() };
        encoder.calls += 1;
        let Some(&flags) = encoder.ready.front() else {
            return MEDIA_PENDING;
        };
        // A rejected packet is offered again with the same sequence.
        let sequence = encoder.delivered + 1;
        let bytes = [sequence as u8];
        let packet = EncodedPacket {
            codec: CODEC_H264,
            flags,
            data: if flags == FLAG_END_OF_STREAM {
                std::ptr::null()
            } else {
                bytes.as_ptr()
            },
            len: if flags == FLAG_END_OF_STREAM { 0 } else { 1 },
            timestamp_us: sequence,
            sequence,
        };
        // SAFETY: packet and bytes stay live for the synchronous callback.
        let status = unsafe { sink.send(&packet) };
        if status == STATUS_OK {
            encoder.ready.pop_front();
            encoder.delivered += 1;
        }
        status
    }
    unsafe extern "C" fn unused_submit(
        _: *mut c_void,
        _: u64,
        _: *const amaterasu_media::CpuFrame,
        _: u32,
    ) -> Status {
        STATUS_INVALID_ARGUMENT
    }
    unsafe extern "C" fn unused_drain(_: *mut c_void, _: u64) -> Status {
        STATUS_INVALID_ARGUMENT
    }

    fn api(encoder: &mut ScriptedEncoder) -> EncoderApi {
        EncoderApi {
            base: NativeInterface {
                header: AbiHeader::of::<EncoderApi>(),
                context: (encoder as *mut ScriptedEncoder).cast(),
                release: None,
            },
            submit: unused_submit,
            receive: scripted_receive,
            drain: unused_drain,
        }
    }

    fn queues() -> Arc<Mutex<Queues>> {
        let queues = Arc::new(Mutex::new(Queues::default()));
        queues.lock().unwrap().open(1, "consumer", 0);
        queues
    }

    fn queued(queues: &Arc<Mutex<Queues>>) -> Vec<u64> {
        let mut sequences = Vec::new();
        unsafe extern "C" fn record(context: *mut c_void, packet: *const EncodedPacket) -> Status {
            // SAFETY: the test lends its vector and the queue lends the packet.
            unsafe { (*context.cast::<Vec<u64>>()).push((*packet).sequence) };
            STATUS_OK
        }
        let sink = PacketSink {
            context: (&mut sequences as *mut Vec<u64>).cast(),
            write: record,
        };
        let mut queues = queues.lock().unwrap();
        // SAFETY: the recording sink copies only the sequence number.
        while unsafe { queues.receive("consumer", 1, sink) } == STATUS_OK {}
        sequences
    }

    /// Hop accounting for encoder -> stream queue: previously one packet per
    /// pump, so a second finished packet waited for the next host pass.
    #[test]
    fn every_finished_packet_reaches_the_stream_queue_in_one_pump() {
        let mut encoder = ScriptedEncoder::new([FLAG_END_OF_ACCESS_UNIT; 3]);
        let api = api(&mut encoder);
        let queues = queues();
        let received = receive(&api, 7, 1, &queues).unwrap();
        assert_eq!(received.packets, 3);
        assert_eq!(queued(&queues), [1, 2, 3]);
        assert_eq!(encoder.calls, 4, "one empty check ends the drain");
    }

    #[test]
    fn stream_queue_backpressure_stops_the_drain_without_losing_output() {
        let mut encoder = ScriptedEncoder::new([FLAG_END_OF_ACCESS_UNIT; 6]);
        let api = api(&mut encoder);
        let queues = queues();
        assert_eq!(receive(&api, 7, 1, &queues).unwrap().packets, 4);
        assert_eq!(encoder.calls, 4, "the budget bounds one pump");
        // The full stream queue rejects the next packet; the encoder keeps it.
        let rejected = receive(&api, 7, 1, &queues).unwrap();
        assert_eq!(rejected.packets, 1);
        assert_eq!(encoder.ready.len(), 2);
        assert_eq!(queued(&queues), [1, 2, 3, 4]);
        assert_eq!(receive(&api, 7, 1, &queues).unwrap().packets, 2);
        assert_eq!(queued(&queues), [5, 6]);
    }

    #[test]
    fn end_of_stream_is_the_last_receive_from_a_drained_encoder() {
        let mut encoder = ScriptedEncoder::new([FLAG_END_OF_ACCESS_UNIT, FLAG_END_OF_STREAM]);
        let api = api(&mut encoder);
        let queues = queues();
        let received = receive(&api, 7, 1, &queues).unwrap();
        assert_eq!(received.packets, 2);
        assert_eq!(encoder.calls, 2, "the drained encoder was polled again");
        assert!(queues.lock().unwrap().eos_queued(1));
    }
}
