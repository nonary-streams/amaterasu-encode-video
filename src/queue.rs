//! Bounded encoded packet ownership shared with native consumer bindings.
use amaterasu_media::{
    EncodedPacket, FLAG_END_OF_STREAM, MAX_PACKET_BYTES, MEDIA_BUSY, MEDIA_CLOSED, MEDIA_PENDING,
    PacketSink, TimedEncodedPacket, TimedPacketSink,
};
use amaterasu_module_sdk::abi::{STATUS_INVALID_ARGUMENT, STATUS_OK, Status};
use amaterasu_module_sdk::diagnostics::LatencyMetric;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;

struct Packet {
    queued_at: Instant,
    bytes: Vec<u8>,
    codec: u32,
    flags: u32,
    timestamp_us: u64,
    sequence: u64,
    processing_started_host_us: u64,
}

struct Stream {
    queue_wait_us: LatencyMetric,
    queue_high_water: usize,
    consumer_retries: u64,
    timing_source: Option<(String, u64, u64)>,
    timing_epoch: u64,
    encode_timing: Option<amaterasu_media::EncodeTimingSnapshotV1>,
    timing_available: bool,
    owner: String,
    /// Host-clock microseconds at the stream's timestamp origin. Packets
    /// leave the queue stamped on the shared host clock (see
    /// `amaterasu_media::host_monotonic_micros`) so consumers can measure
    /// host latency; inside video they stay relative to the stream's open.
    clock_origin_us: u64,
    geometry: Option<amaterasu_media::CaptureGeometry>,
    hdr: Option<amaterasu_media::HdrMetadata>,
    packets: VecDeque<Packet>,
    bytes: usize,
    eos_queued: bool,
    eos_accepted: bool,
    /// A bounded association for accepted frames still in the encoder. Older
    /// entries may be evicted when a replacement provider delays output; their
    /// processing time then becomes explicitly unavailable rather than guessed.
    processing_starts: BTreeMap<u64, (u64, u64)>,
    /// Freeze the first fragment's choice so later fragments remain consistent
    /// even if a concurrent bookkeeping miss invalidates provisional starts.
    frozen_processing_starts: BTreeMap<u64, u64>,
}

#[derive(Default)]
pub struct Queues {
    streams: BTreeMap<u64, Stream>,
    processing_generation: Arc<AtomicU64>,
    frame_trace: Option<crate::frame_trace::FrameTrace>,
}

impl Queues {
    pub fn trace_frames(&mut self, trace: crate::frame_trace::FrameTrace) {
        self.frame_trace = Some(trace);
    }

    pub fn processing_generation(&self) -> Arc<AtomicU64> {
        self.processing_generation.clone()
    }

    pub fn clear(&mut self) {
        self.streams.clear();
    }

    pub fn open(&mut self, id: u64, owner: &str, clock_origin_us: u64) {
        self.streams.insert(
            id,
            Stream {
                queue_wait_us: LatencyMetric::default(),
                queue_high_water: 0,
                consumer_retries: 0,
                timing_source: None,
                timing_epoch: 0,
                encode_timing: None,
                timing_available: false,
                owner: owner.into(),
                clock_origin_us,
                geometry: None,
                hdr: None,
                packets: VecDeque::new(),
                bytes: 0,
                eos_queued: false,
                eos_accepted: false,
                processing_starts: BTreeMap::new(),
                frozen_processing_starts: BTreeMap::new(),
            },
        );
    }

    pub fn set_geometry(&mut self, id: u64, geometry: Option<amaterasu_media::CaptureGeometry>) {
        if let Some(stream) = self.streams.get_mut(&id) {
            stream.geometry = geometry;
        }
    }

    /// Preserve the selected provider's measurements and translate its epoch to
    /// this video's lifetime. A replacement never joins two unrelated counters.
    pub fn observe_encode_timing(
        &mut self,
        id: u64,
        provider: &str,
        session: u64,
        snapshot: Option<amaterasu_media::EncodeTimingSnapshotV1>,
    ) {
        let Some(stream) = self.streams.get_mut(&id) else {
            return;
        };
        stream.timing_available = false;
        let Some(mut snapshot) = snapshot.filter(|snapshot| snapshot.validate().is_ok()) else {
            return;
        };
        let source = (provider.to_owned(), session, snapshot.epoch);
        if stream.timing_source.as_ref() != Some(&source) {
            let Some(epoch) = stream.timing_epoch.checked_add(1) else {
                return;
            };
            stream.timing_epoch = epoch;
            stream.timing_source = Some(source);
            stream.encode_timing = None;
        }
        snapshot.epoch = stream.timing_epoch;
        if stream.encode_timing.is_some_and(|previous| {
            !amaterasu_module_sdk::media::timing_follows(snapshot, previous)
        }) {
            return;
        }
        stream.encode_timing = Some(snapshot);
        stream.timing_available = true;
    }

    pub fn encode_timing(
        &self,
        owner: &str,
        id: u64,
    ) -> Result<Option<amaterasu_media::EncodeTimingSnapshotV1>, Status> {
        self.streams
            .get(&id)
            .filter(|stream| stream.owner == owner)
            .map(|stream| stream.encode_timing.filter(|_| stream.timing_available))
            .ok_or(MEDIA_CLOSED)
    }

    pub fn geometry(
        &self,
        owner: &str,
        id: u64,
    ) -> Result<Option<amaterasu_media::CaptureGeometry>, Status> {
        self.streams
            .get(&id)
            .filter(|stream| stream.owner == owner)
            .map(|stream| stream.geometry)
            .ok_or(MEDIA_CLOSED)
    }

    pub fn set_hdr(&mut self, id: u64, hdr: Option<amaterasu_media::HdrMetadata>) {
        if let Some(stream) = self.streams.get_mut(&id) {
            stream.hdr = hdr;
        }
    }
    pub fn hdr(
        &self,
        owner: &str,
        id: u64,
    ) -> Result<Option<amaterasu_media::HdrMetadata>, Status> {
        self.streams
            .get(&id)
            .filter(|stream| stream.owner == owner)
            .map(|stream| stream.hdr)
            .ok_or(MEDIA_CLOSED)
    }
    pub fn close(&mut self, id: u64) {
        self.streams.remove(&id);
    }

    pub fn eos_queued(&self, id: u64) -> bool {
        self.streams
            .get(&id)
            .is_some_and(|stream| stream.eos_queued)
    }

    pub fn eos_accepted(&self, id: u64) -> bool {
        self.streams
            .get(&id)
            .is_some_and(|stream| stream.eos_accepted)
    }

    /// Record provisionally before a pump; output can arrive in the same pump.
    pub fn start_processing(&mut self, id: u64, sequence: u64, started_host_us: u64) {
        const MAX_PENDING_TIMESTAMPS: usize = 128;
        if let Some(stream) = self.streams.get_mut(&id) {
            let generation = self.processing_generation.load(Ordering::Relaxed);
            stream
                .processing_starts
                .insert(sequence, (started_host_us, generation));
            while stream.processing_starts.len() > MAX_PENDING_TIMESTAMPS {
                stream.processing_starts.pop_first();
            }
        }
    }

    /// A rejected or deferred submission does not own this sequence.
    pub fn cancel_processing(&mut self, id: u64, sequence: u64) {
        if let Some(stream) = self.streams.get_mut(&id) {
            stream.processing_starts.remove(&sequence);
        }
    }

    /// # Safety
    /// Packet memory must be readable until this synchronous copy returns.
    pub unsafe fn push(&mut self, id: u64, packet: &EncodedPacket) -> Status {
        let Some(stream) = self.streams.get_mut(&id) else {
            return MEDIA_CLOSED;
        };
        if stream.eos_queued {
            return MEDIA_CLOSED;
        }
        if let Err(status) = packet.validate() {
            return status;
        }
        if stream
            .clock_origin_us
            .checked_add(packet.timestamp_us)
            .is_none()
        {
            return STATUS_INVALID_ARGUMENT;
        }
        if stream.packets.len() >= 4 || stream.bytes.saturating_add(packet.len) > MAX_PACKET_BYTES {
            return MEDIA_BUSY;
        }
        // SAFETY: provider retains packet for the whole sink invocation.
        let bytes = match unsafe { packet.bytes() } {
            Ok(bytes) => bytes.to_vec(),
            Err(status) => return status,
        };
        stream.eos_queued = packet.flags == FLAG_END_OF_STREAM;
        stream.bytes += bytes.len();
        let processing_started_host_us =
            if let Some(value) = stream.frozen_processing_starts.get(&packet.sequence) {
                *value
            } else {
                let value = stream
                    .processing_starts
                    .get(&packet.sequence)
                    .and_then(|&(started, generation)| {
                        let current = self.processing_generation.load(Ordering::Relaxed);
                        (current != u64::MAX && generation == current).then_some(started)
                    })
                    .unwrap_or(0);
                stream
                    .frozen_processing_starts
                    .insert(packet.sequence, value);
                while stream.frozen_processing_starts.len() > 128 {
                    stream.frozen_processing_starts.pop_first();
                }
                // This branch runs once per frame, on its first fragment.
                if packet.flags != FLAG_END_OF_STREAM
                    && let Some(trace) = &self.frame_trace
                {
                    trace.packet_queued(id, packet.sequence);
                }
                value
            };
        stream.packets.push_back(Packet {
            queued_at: Instant::now(),
            bytes,
            codec: packet.codec,
            flags: packet.flags,
            timestamp_us: packet.timestamp_us,
            sequence: packet.sequence,
            processing_started_host_us,
        });
        stream.queue_high_water = stream.queue_high_water.max(stream.packets.len());
        STATUS_OK
    }

    pub fn take_timing(&mut self, id: u64) -> Option<serde_json::Value> {
        let stream = self.streams.get_mut(&id)?;
        let wait = std::mem::take(&mut stream.queue_wait_us);
        let high_water = std::mem::replace(&mut stream.queue_high_water, stream.packets.len());
        let retries = std::mem::take(&mut stream.consumer_retries);
        Some(
            serde_json::json!({"queue_wait_us":wait,"queue_high_water":high_water,
            "consumer_retries":retries,"queued_packets":stream.packets.len(),"queued_bytes":stream.bytes,
            "oldest_queued_us":stream.packets.front().map(|packet| amaterasu_module_sdk::diagnostics::elapsed_us(packet.queued_at)),
            "scope":"encoded_packet_enqueue_to_consumer_callback_entry"}),
        )
    }

    /// # Safety
    /// Sink follows borrowed packet rules and must not reenter this queue.
    pub unsafe fn receive(&mut self, owner: &str, id: u64, sink: PacketSink) -> Status {
        let Some(stream) = self
            .streams
            .get_mut(&id)
            .filter(|stream| stream.owner == owner)
        else {
            return MEDIA_CLOSED;
        };
        let Some(packet) = stream.packets.front() else {
            return if stream.eos_accepted {
                MEDIA_CLOSED
            } else {
                MEDIA_PENDING
            };
        };
        let borrowed = EncodedPacket {
            codec: packet.codec,
            flags: packet.flags,
            data: packet.bytes.as_ptr(),
            len: packet.bytes.len(),
            timestamp_us: stream.clock_origin_us + packet.timestamp_us,
            sequence: packet.sequence,
        };
        // SAFETY: queue owns packet during callback; rejection retains it.
        let callback_started = Instant::now();
        let status = unsafe { sink.send(&borrowed) };
        if status == STATUS_OK {
            stream.queue_wait_us.record(
                u64::try_from(
                    callback_started
                        .saturating_duration_since(packet.queued_at)
                        .as_micros(),
                )
                .unwrap_or(u64::MAX),
            );
            stream.eos_accepted = packet.flags == FLAG_END_OF_STREAM;
            stream.bytes -= packet.bytes.len();
            stream.packets.pop_front();
        } else {
            stream.consumer_retries = stream.consumer_retries.saturating_add(1);
        }
        status
    }

    /// # Safety
    /// Sink follows borrowed packet rules and must not reenter this queue.
    pub unsafe fn receive_timed(&mut self, owner: &str, id: u64, sink: TimedPacketSink) -> Status {
        let Some(stream) = self
            .streams
            .get_mut(&id)
            .filter(|stream| stream.owner == owner)
        else {
            return MEDIA_CLOSED;
        };
        let Some(packet) = stream.packets.front() else {
            return if stream.eos_accepted {
                MEDIA_CLOSED
            } else {
                MEDIA_PENDING
            };
        };
        let borrowed = TimedEncodedPacket {
            base: EncodedPacket {
                codec: packet.codec,
                flags: packet.flags,
                data: packet.bytes.as_ptr(),
                len: packet.bytes.len(),
                timestamp_us: stream.clock_origin_us + packet.timestamp_us,
                sequence: packet.sequence,
            },
            processing_started_host_us: packet.processing_started_host_us,
        };
        // SAFETY: queue owns packet bytes for the callback; rejection retains it.
        let callback_started = Instant::now();
        let status = unsafe { sink.send(&borrowed) };
        if status == STATUS_OK {
            stream.queue_wait_us.record(
                u64::try_from(
                    callback_started
                        .saturating_duration_since(packet.queued_at)
                        .as_micros(),
                )
                .unwrap_or(u64::MAX),
            );
            stream.eos_accepted = packet.flags == FLAG_END_OF_STREAM;
            stream.bytes -= packet.bytes.len();
            stream.packets.pop_front();
        } else {
            stream.consumer_retries = stream.consumer_retries.saturating_add(1);
        }
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amaterasu_media::CODEC_H264;
    use std::ffi::c_void;

    #[derive(Default)]
    struct TimedReceipt {
        values: Vec<(u64, u64, u64)>,
        busy: bool,
    }

    unsafe extern "C" fn timed_receipt(
        context: *mut c_void,
        packet: *const TimedEncodedPacket,
    ) -> Status {
        // SAFETY: the test provides a live receipt and queue-borrowed packet.
        let receipt = unsafe { &mut *context.cast::<TimedReceipt>() };
        let packet = unsafe { &*packet };
        receipt.values.push((
            packet.base.sequence,
            packet.base.timestamp_us,
            packet.processing_started_host_us,
        ));
        if receipt.busy { MEDIA_BUSY } else { STATUS_OK }
    }

    #[test]
    fn timed_delivery_preserves_independent_timestamp_across_fragments_and_retry() {
        let mut queues = Queues::default();
        queues.open(1, "alice", 1000);
        queues.start_processing(1, 7, 5000);
        let data = [1u8];
        let fragment = EncodedPacket {
            codec: CODEC_H264,
            flags: 0,
            data: data.as_ptr(),
            len: 1,
            timestamp_us: 99,
            sequence: 7,
        };
        // SAFETY: test data remains live until both copies complete.
        assert_eq!(unsafe { queues.push(1, &fragment) }, STATUS_OK);
        // A bookkeeping miss from another stream must not split metadata in
        // the middle of this access unit.
        queues
            .processing_generation()
            .fetch_add(1, Ordering::Relaxed);
        assert_eq!(unsafe { queues.push(1, &fragment) }, STATUS_OK);
        let mut receipt = TimedReceipt {
            busy: true,
            ..Default::default()
        };
        let sink = TimedPacketSink {
            context: (&mut receipt as *mut TimedReceipt).cast(),
            write: timed_receipt,
        };
        // SAFETY: receipt and packet remain alive during each synchronous callback.
        assert_eq!(
            unsafe { queues.receive_timed("alice", 1, sink) },
            MEDIA_BUSY
        );
        let queued = queues.take_timing(1).unwrap();
        assert_eq!(queued["queue_wait_us"]["calls"], 0);
        assert_eq!(queued["consumer_retries"], 1);
        assert_eq!(queued["queued_packets"], 2);
        receipt.busy = false;
        assert_eq!(unsafe { queues.receive_timed("alice", 1, sink) }, STATUS_OK);
        assert_eq!(unsafe { queues.receive_timed("alice", 1, sink) }, STATUS_OK);
        assert_eq!(receipt.values, vec![(7, 1099, 5000); 3]);
        assert_eq!(queues.take_timing(1).unwrap()["queue_wait_us"]["calls"], 2);
        assert_eq!(queues.take_timing(1).unwrap()["queue_wait_us"]["calls"], 0);
    }

    #[test]
    fn absent_cancelled_and_evicted_timestamps_are_unknown_and_bounded() {
        let mut queues = Queues::default();
        queues.open(1, "alice", 1000);
        queues.start_processing(1, 1, 5000);
        queues.cancel_processing(1, 1);
        for sequence in 2..=130 {
            queues.start_processing(1, sequence, sequence + 5000);
        }
        assert_eq!(queues.streams[&1].processing_starts.len(), 128);
        let data = [1u8];
        for sequence in [1, 2] {
            let packet = EncodedPacket {
                codec: CODEC_H264,
                flags: 0,
                data: data.as_ptr(),
                len: 1,
                timestamp_us: 99,
                sequence,
            };
            // SAFETY: input remains live until copied.
            assert_eq!(unsafe { queues.push(1, &packet) }, STATUS_OK);
        }
        let mut receipt = TimedReceipt::default();
        let sink = TimedPacketSink {
            context: (&mut receipt as *mut TimedReceipt).cast(),
            write: timed_receipt,
        };
        // SAFETY: receipt and packet remain alive during each synchronous callback.
        assert_eq!(unsafe { queues.receive_timed("alice", 1, sink) }, STATUS_OK);
        assert_eq!(unsafe { queues.receive_timed("alice", 1, sink) }, STATUS_OK);
        assert_eq!(receipt.values, vec![(1, 1099, 0), (2, 1099, 0)]);
    }

    #[test]
    fn invalidation_rejects_stale_associations_across_streams_and_saturation() {
        let mut queues = Queues::default();
        queues.open(1, "alice", 0);
        queues.open(2, "alice", 0);
        queues.start_processing(1, 1, 100);
        queues.start_processing(2, 1, 200);
        let generation = queues.processing_generation();
        generation.fetch_add(1, Ordering::Relaxed);
        // A retry for stream 1 is a distinct submission and gets a fresh clock.
        queues.start_processing(1, 1, 300);
        let data = [1u8];
        let packet = EncodedPacket {
            codec: CODEC_H264,
            flags: 0,
            data: data.as_ptr(),
            len: 1,
            timestamp_us: 1,
            sequence: 1,
        };
        // SAFETY: packet bytes remain live until copied.
        assert_eq!(unsafe { queues.push(1, &packet) }, STATUS_OK);
        assert_eq!(unsafe { queues.push(2, &packet) }, STATUS_OK);
        let mut receipt = TimedReceipt::default();
        let sink = TimedPacketSink {
            context: (&mut receipt as *mut TimedReceipt).cast(),
            write: timed_receipt,
        };
        // SAFETY: sink and data remain live until callback returns.
        assert_eq!(unsafe { queues.receive_timed("alice", 1, sink) }, STATUS_OK);
        assert_eq!(unsafe { queues.receive_timed("alice", 2, sink) }, STATUS_OK);
        assert_eq!(receipt.values, vec![(1, 1, 300), (1, 1, 0)]);
        generation.store(u64::MAX, Ordering::Relaxed);
        queues.start_processing(1, 2, 400);
        let last = EncodedPacket {
            sequence: 2,
            ..packet
        };
        // SAFETY: packet bytes remain live until copied and callback returns.
        assert_eq!(unsafe { queues.push(1, &last) }, STATUS_OK);
        assert_eq!(unsafe { queues.receive_timed("alice", 1, sink) }, STATUS_OK);
        assert_eq!(receipt.values.last(), Some(&(2, 1, 0)));
    }
    unsafe extern "C" fn reject(_: *mut c_void, _: *const EncodedPacket) -> Status {
        MEDIA_BUSY
    }
    unsafe extern "C" fn accept(_: *mut c_void, packet: *const EncodedPacket) -> Status {
        // SAFETY: queue borrows this packet for the callback.
        assert_eq!(unsafe { (*packet).bytes() }.unwrap(), &[1, 2, 3]);
        STATUS_OK
    }
    #[test]
    fn rejects_timestamp_that_overflows_the_negotiated_host_epoch() {
        let mut queues = Queues::default();
        queues.open(1, "alice", u64::MAX - 1);
        let bytes = [1];
        let packet = EncodedPacket {
            codec: CODEC_H264,
            flags: 0,
            data: bytes.as_ptr(),
            len: 1,
            timestamp_us: 2,
            sequence: 1,
        };
        assert_eq!(unsafe { queues.push(1, &packet) }, STATUS_INVALID_ARGUMENT);
        assert_eq!(
            unsafe {
                queues.receive(
                    "alice",
                    1,
                    PacketSink {
                        context: std::ptr::null_mut(),
                        write: reject,
                    },
                )
            },
            MEDIA_PENDING
        );
    }
    #[test]
    fn bounded_queue_preserves_rejected_packet_and_enforces_owner() {
        let mut queues = Queues::default();
        queues.open(1, "alice", 0);
        let bytes = [1, 2, 3];
        let packet = EncodedPacket {
            codec: CODEC_H264,
            flags: 0,
            data: bytes.as_ptr(),
            len: bytes.len(),
            timestamp_us: 1,
            sequence: 1,
        };
        for _ in 0..4 {
            assert_eq!(unsafe { queues.push(1, &packet) }, STATUS_OK);
        }
        assert_eq!(unsafe { queues.push(1, &packet) }, MEDIA_BUSY);
        let rejected = PacketSink {
            context: std::ptr::null_mut(),
            write: reject,
        };
        assert_eq!(unsafe { queues.receive("bob", 1, rejected) }, MEDIA_CLOSED);
        assert_eq!(unsafe { queues.receive("alice", 1, rejected) }, MEDIA_BUSY);
        assert_eq!(queues.streams[&1].packets.len(), 4);
        let accepted = PacketSink {
            context: std::ptr::null_mut(),
            write: accept,
        };
        assert_eq!(unsafe { queues.receive("alice", 1, accepted) }, STATUS_OK);
        assert_eq!(queues.streams[&1].packets.len(), 3);
        assert_eq!(queues.streams[&1].bytes, 9);
        queues.close(1);
        assert_eq!(
            unsafe { queues.receive("alice", 1, accepted) },
            MEDIA_CLOSED
        );
    }
    #[test]
    fn byte_budget_rejects_before_copying_another_packet() {
        let mut queues = Queues::default();
        queues.open(1, "alice", 0);
        let bytes = vec![1; MAX_PACKET_BYTES];
        let packet = EncodedPacket {
            codec: CODEC_H264,
            flags: 0,
            data: bytes.as_ptr(),
            len: bytes.len(),
            timestamp_us: 1,
            sequence: 1,
        };
        assert_eq!(unsafe { queues.push(1, &packet) }, STATUS_OK);
        assert_eq!(unsafe { queues.push(1, &packet) }, MEDIA_BUSY);
        assert_eq!(queues.streams[&1].packets.len(), 1);
    }
    #[test]
    fn eos_waits_for_fifo_capacity_and_is_delivered_only_once() {
        unsafe extern "C" fn count(context: *mut c_void, packet: *const EncodedPacket) -> Status {
            // SAFETY: test sink borrows a counter and packet for this call.
            if unsafe { (*packet).flags } == FLAG_END_OF_STREAM {
                unsafe {
                    *context.cast::<usize>() += 1;
                }
            }
            STATUS_OK
        }
        let mut queues = Queues::default();
        queues.open(1, "alice", 0);
        let bytes = [1];
        let packet = EncodedPacket {
            codec: CODEC_H264,
            flags: 0,
            data: bytes.as_ptr(),
            len: 1,
            timestamp_us: 1,
            sequence: 1,
        };
        for _ in 0..4 {
            assert_eq!(unsafe { queues.push(1, &packet) }, STATUS_OK);
        }
        let eos = EncodedPacket {
            flags: FLAG_END_OF_STREAM,
            data: std::ptr::null(),
            len: 0,
            ..packet
        };
        assert_eq!(unsafe { queues.push(1, &eos) }, MEDIA_BUSY);
        let mut count_value = 0usize;
        let sink = PacketSink {
            context: (&mut count_value as *mut usize).cast(),
            write: count,
        };
        assert_eq!(unsafe { queues.receive("alice", 1, sink) }, STATUS_OK);
        assert_eq!(unsafe { queues.push(1, &eos) }, STATUS_OK);
        assert_eq!(unsafe { queues.push(1, &eos) }, MEDIA_CLOSED);
        for _ in 0..4 {
            assert_eq!(unsafe { queues.receive("alice", 1, sink) }, STATUS_OK);
        }
        assert_eq!(count_value, 1);
        assert!(queues.eos_accepted(1));
        assert_eq!(unsafe { queues.receive("alice", 1, sink) }, MEDIA_CLOSED);
    }
}

#[cfg(test)]
mod geometry_tests {
    use super::*;
    use amaterasu_media::{CaptureGeometry, DesktopRect};
    #[test]
    fn timing_tracks_only_the_selected_provider_and_rebases_replacements() {
        use amaterasu_media::EncodeTimingSnapshotV1;
        let mut queues = Queues::default();
        queues.open(1, "owner", 0);
        let mut first = EncodeTimingSnapshotV1::empty(1);
        first.record(2000);
        queues.observe_encode_timing(1, "provider-a", 1, Some(first));
        let initial = queues.encode_timing("owner", 1).unwrap().unwrap();
        assert_eq!(initial.completed_frames, 1);
        assert_eq!(queues.encode_timing("another-owner", 1), Err(MEDIA_CLOSED));
        queues.observe_encode_timing(1, "provider-a", 1, Some(EncodeTimingSnapshotV1::empty(1)));
        assert_eq!(
            queues.encode_timing("owner", 1).unwrap(),
            None,
            "backwards counters must not become a measurement"
        );
        queues.observe_encode_timing(1, "provider-a", 1, None);
        assert_eq!(queues.encode_timing("owner", 1).unwrap(), None);
        let mut replacement = EncodeTimingSnapshotV1::empty(1);
        replacement.record(5000);
        queues.observe_encode_timing(1, "provider-b", 1, Some(replacement));
        let replaced = queues.encode_timing("owner", 1).unwrap().unwrap();
        assert_ne!(initial.epoch, replaced.epoch);
        assert_eq!(replaced.mean_ms_since(initial), None);
        assert_eq!(replaced.total_us, 5000);
    }
    #[test]
    fn geometry_tracks_the_owned_stream_and_peer_topology_without_crossing_callers() {
        let mut queues = Queues::default();
        queues.open(1, "alice", 0);
        let first = CaptureGeometry::new(
            DesktopRect::new(1536, 0, 1920, 1080).unwrap(),
            DesktopRect::new(0, 0, 3456, 1080).unwrap(),
        )
        .unwrap();
        queues.set_geometry(1, Some(first));
        assert_eq!(queues.geometry("alice", 1).unwrap(), Some(first));
        assert_eq!(queues.geometry("bob", 1), Err(MEDIA_CLOSED));
        let expanded =
            CaptureGeometry::new(first.output, DesktopRect::new(0, 0, 5376, 1080).unwrap())
                .unwrap();
        queues.set_geometry(1, Some(expanded));
        assert_eq!(queues.geometry("alice", 1).unwrap(), Some(expanded));
        queues.close(1);
        assert_eq!(queues.geometry("alice", 1), Err(MEDIA_CLOSED));
    }

    #[test]
    fn packets_leave_on_the_host_clock() {
        use amaterasu_media::{CODEC_H264, FLAG_KEYFRAME};
        use std::ffi::c_void;
        let mut queues = Queues::default();
        queues.open(1, "alice", 1_000_000);
        let data = [0u8, 0, 0, 1, 0x65];
        let packet = EncodedPacket {
            codec: CODEC_H264,
            flags: FLAG_KEYFRAME,
            data: data.as_ptr(),
            len: data.len(),
            timestamp_us: 250,
            sequence: 1,
        };
        // SAFETY: the packet bytes outlive the synchronous copy.
        assert_eq!(unsafe { queues.push(1, &packet) }, STATUS_OK);
        unsafe extern "C" fn record(context: *mut c_void, packet: *const EncodedPacket) -> Status {
            // SAFETY: the test passes a live u64 and a packet valid for this call.
            unsafe { *context.cast::<u64>() = (*packet).timestamp_us };
            STATUS_OK
        }
        let mut seen = 0u64;
        let sink = PacketSink {
            context: (&mut seen as *mut u64).cast(),
            write: record,
        };
        // SAFETY: the sink and its context outlive this synchronous receive.
        assert_eq!(unsafe { queues.receive("alice", 1, sink) }, STATUS_OK);
        assert_eq!(seen, 1_000_250);
    }
}
