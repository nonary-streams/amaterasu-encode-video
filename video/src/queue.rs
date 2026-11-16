//! Bounded encoded packet ownership shared with native consumer bindings.
use amaterasu_media::{
    ENCODER_CHUNK_ACCESS_UNIT_END, ENCODER_CHUNK_ACCESS_UNIT_START, ENCODER_CHUNK_END_OF_STREAM,
    ENCODER_CHUNK_KEYFRAME, EncodedPacket, EncoderFormatId, EncoderOutputChunk,
    FLAG_END_OF_ACCESS_UNIT, FLAG_END_OF_STREAM, FLAG_KEYFRAME, FRAME_CLOCK_STREAM_RELATIVE,
    FrameClockMetadata, MAX_PACKET_BYTES, MAX_VIDEO_ACCESS_UNIT_BYTES,
    MAX_VIDEO_ACCESS_UNIT_FRAGMENTS, MEDIA_BUSY, MEDIA_CLOSED, MEDIA_PENDING, NegotiatedVideoUnit,
    NegotiatedVideoUnitSink, PacketSink, TimedEncodedPacket, TimedPacketSink,
    VIDEO_UNIT_END_OF_STREAM, VIDEO_UNIT_KEYFRAME, VIDEO_UNIT_VERSION,
};
use amaterasu_module_sdk::abi::{BorrowedBytes, STATUS_INVALID_ARGUMENT, STATUS_OK, Status};
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

// Startup proof tracks only bounded metadata. Encoded bytes remain owned by
// the normal queue; no capture frame or provider memory is retained here.
#[derive(Default)]
struct StartupFrame {
    access_unit: Option<StartupAccessUnit>,
    ready: bool,
}

struct StartupAccessUnit {
    codec: u32,
    timestamp_us: u64,
    sequence: u64,
    fragments: usize,
    keyframe: bool,
}

impl StartupFrame {
    fn accepts(&self, packet: &EncodedPacket) -> bool {
        if self.ready {
            return true;
        }
        match &self.access_unit {
            None => true,
            Some(unit) => {
                packet.flags != FLAG_END_OF_STREAM
                    && unit.codec == packet.codec
                    && unit.timestamp_us == packet.timestamp_us
                    && unit.sequence == packet.sequence
                    && unit.fragments < 1024
            }
        }
    }

    fn observe(&mut self, packet: &EncodedPacket) {
        if self.ready || packet.flags == FLAG_END_OF_STREAM {
            return;
        }
        let unit = self.access_unit.get_or_insert(StartupAccessUnit {
            codec: packet.codec,
            timestamp_us: packet.timestamp_us,
            sequence: packet.sequence,
            fragments: 0,
            keyframe: false,
        });
        unit.fragments += 1;
        unit.keyframe |= packet.flags & FLAG_KEYFRAME != 0;
        if packet.flags & FLAG_END_OF_ACCESS_UNIT != 0 {
            self.ready = unit.keyframe;
            self.access_unit = None;
        }
    }
}

struct Stream {
    startup_frame: Option<StartupFrame>,
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
    negotiated: Option<UnitQueue>,
}

/// Authenticated session composition fixes these values before output begins.
#[derive(Clone, Copy)]
pub struct UnitSelection {
    pub stream: u64,
    pub generation: u64,
    pub offer_id: u64,
    pub source_id: u64,
    pub config_generation: u64,
    pub codec: EncoderFormatId,
    pub framing: EncoderFormatId,
    pub clock_epoch: [u8; 16],
}

struct Unit {
    access_unit_id: u64,
    submission_id: u64,
    timestamp_us: u64,
    flags: u32,
    bytes: Vec<u8>,
    fragments: usize,
}

struct UnitQueue {
    selection: UnitSelection,
    current: Option<Unit>,
    ready: VecDeque<Unit>,
    bytes: usize,
    eos_queued: bool,
    eos_accepted: bool,
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
                startup_frame: None,
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
                negotiated: None,
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

    pub fn configure_units(&mut self, id: u64, selection: UnitSelection) -> Result<(), Status> {
        let stream = self.streams.get_mut(&id).ok_or(MEDIA_CLOSED)?;
        let valid = |id: EncoderFormatId| id.namespace_id != 0 && id.value != 0 && id.schema != 0;
        if selection.stream != id
            || selection.generation == 0
            || selection.offer_id == 0
            || selection.source_id == 0
            || selection.config_generation == 0
            || selection.clock_epoch == [0; 16]
            || !valid(selection.codec)
            || !valid(selection.framing)
            || stream.negotiated.is_some()
            || !stream.packets.is_empty()
            || stream.eos_queued
        {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        stream.negotiated = Some(UnitQueue {
            selection,
            current: None,
            ready: VecDeque::new(),
            bytes: 0,
            eos_queued: false,
            eos_accepted: false,
        });
        Ok(())
    }

    /// # Safety
    /// The encoder owns initialized chunk bytes throughout this synchronous call.
    pub unsafe fn push_unit_chunk(&mut self, id: u64, chunk: &EncoderOutputChunk) -> Status {
        let Some(queue) = self
            .streams
            .get_mut(&id)
            .and_then(|s| s.negotiated.as_mut())
        else {
            return MEDIA_CLOSED;
        };
        if queue.eos_queued {
            return MEDIA_CLOSED;
        }
        let selection = queue.selection;
        let same = |a: EncoderFormatId, b: EncoderFormatId| {
            a.namespace_id == b.namespace_id && a.value == b.value && a.schema == b.schema
        };
        let flags = chunk.flags;
        let eos = flags == ENCODER_CHUNK_END_OF_STREAM;
        let start = flags & ENCODER_CHUNK_ACCESS_UNIT_START != 0;
        let end = flags & ENCODER_CHUNK_ACCESS_UNIT_END != 0;
        let picture = flags & ENCODER_CHUNK_KEYFRAME != 0;
        if chunk.size < std::mem::size_of::<EncoderOutputChunk>() as u32
            || chunk.source_id != selection.source_id
            || chunk.config_generation != selection.config_generation
            || !same(chunk.codec, selection.codec)
            || !same(chunk.framing, selection.framing)
            || flags
                & !(ENCODER_CHUNK_ACCESS_UNIT_START
                    | ENCODER_CHUNK_ACCESS_UNIT_END
                    | ENCODER_CHUNK_KEYFRAME
                    | ENCODER_CHUNK_END_OF_STREAM)
                != 0
            || (flags & ENCODER_CHUNK_END_OF_STREAM != 0 && !eos)
            || (eos && (chunk.bytes.len != 0 || queue.current.is_some()))
            || (!eos
                && (chunk.access_unit_id == 0
                    || chunk.bytes.len == 0
                    || chunk.bytes.len > MAX_PACKET_BYTES
                    || chunk.bytes.ptr.is_null()))
        {
            return STATUS_INVALID_ARGUMENT;
        }
        if queue
            .current
            .as_ref()
            .map_or(0, |unit| unit.bytes.len())
            .checked_add(chunk.bytes.len)
            .is_none_or(|n| n > MAX_VIDEO_ACCESS_UNIT_BYTES)
        {
            return STATUS_INVALID_ARGUMENT;
        }
        if queue.ready.len() >= 4
            || queue
                .bytes
                .checked_add(chunk.bytes.len)
                .is_none_or(|n| n > MAX_VIDEO_ACCESS_UNIT_BYTES)
        {
            return MEDIA_BUSY;
        }
        match &queue.current {
            Some(unit)
                if eos
                    || start
                    || unit.access_unit_id != chunk.access_unit_id
                    || unit.submission_id != chunk.submission_id
                    || unit.timestamp_us != chunk.timestamp_us
                    || (unit.flags == VIDEO_UNIT_KEYFRAME) != picture
                    || unit.fragments >= MAX_VIDEO_ACCESS_UNIT_FRAGMENTS =>
            {
                return STATUS_INVALID_ARGUMENT;
            }
            None if !eos && !start => return STATUS_INVALID_ARGUMENT,
            _ => {}
        }
        let bytes = if chunk.bytes.len == 0 {
            &[][..]
        } else {
            // SAFETY: caller guarantees this chunk allocation remains live.
            unsafe { std::slice::from_raw_parts(chunk.bytes.ptr, chunk.bytes.len) }
        };
        if eos {
            queue.eos_queued = true;
            queue.ready.push_back(Unit {
                access_unit_id: chunk.access_unit_id,
                submission_id: chunk.submission_id,
                timestamp_us: chunk.timestamp_us,
                flags: VIDEO_UNIT_END_OF_STREAM,
                bytes: Vec::new(),
                fragments: 0,
            });
            return STATUS_OK;
        }
        let unit = queue.current.get_or_insert_with(|| Unit {
            access_unit_id: chunk.access_unit_id,
            submission_id: chunk.submission_id,
            timestamp_us: chunk.timestamp_us,
            flags: if picture { VIDEO_UNIT_KEYFRAME } else { 0 },
            bytes: Vec::new(),
            fragments: 0,
        });
        unit.bytes.extend_from_slice(bytes);
        unit.fragments += 1;
        queue.bytes += bytes.len();
        if end {
            queue
                .ready
                .push_back(queue.current.take().expect("unit initialized"));
            if let Some(trace) = &self.frame_trace {
                trace.packet_queued(id, chunk.access_unit_id);
            }
        }
        STATUS_OK
    }

    /// # Safety
    /// Sink must obey the synchronous borrowed-unit contract and not reenter.
    pub unsafe fn receive_unit(
        &mut self,
        owner: &str,
        id: u64,
        sink: NegotiatedVideoUnitSink,
    ) -> Status {
        let Some(queue) = self
            .streams
            .get_mut(&id)
            .filter(|s| s.owner == owner)
            .and_then(|s| s.negotiated.as_mut())
        else {
            return MEDIA_CLOSED;
        };
        let Some(unit) = queue.ready.front() else {
            return if queue.eos_accepted {
                MEDIA_CLOSED
            } else {
                MEDIA_PENDING
            };
        };
        let s = queue.selection;
        let header = NegotiatedVideoUnit {
            size: std::mem::size_of::<NegotiatedVideoUnit>() as u32,
            version: VIDEO_UNIT_VERSION,
            stream: s.stream,
            generation: s.generation,
            offer_id: s.offer_id,
            codec: s.codec,
            framing: s.framing,
            access_unit_id: unit.access_unit_id,
            submission_id: unit.submission_id,
            source_id: s.source_id,
            config_generation: s.config_generation,
            timestamp_us: unit.timestamp_us,
            clock: FrameClockMetadata {
                size: std::mem::size_of::<FrameClockMetadata>() as u32,
                version: 1,
                domain: FRAME_CLOCK_STREAM_RELATIVE,
                reserved: 0,
                epoch_id: s.clock_epoch,
            },
            flags: unit.flags,
            reserved: 0,
            data: BorrowedBytes::new(&unit.bytes),
        };
        // SAFETY: queue retains header and bytes during this synchronous callback.
        let status = unsafe { (sink.write)(sink.context, &header) };
        if status == STATUS_OK {
            queue.eos_accepted = unit.flags == VIDEO_UNIT_END_OF_STREAM;
            queue.bytes -= unit.bytes.len();
            queue.ready.pop_front();
        }
        status
    }

    /// Opt in only when the encoder promised explicit AU boundaries. Legacy
    /// packet consumers keep their existing fragment and EOS semantics.
    pub fn enable_startup_readiness(&mut self, id: u64) {
        if let Some(stream) = self.streams.get_mut(&id) {
            stream.startup_frame = Some(StartupFrame::default());
        }
    }

    /// Local encoder output proof, never client receipt or presentation.
    pub fn startup_frame_ready(&self, owner: &str, id: u64) -> Result<bool, Status> {
        self.streams
            .get(&id)
            .filter(|stream| stream.owner == owner)
            .map(|stream| {
                stream
                    .startup_frame
                    .as_ref()
                    .is_some_and(|frame| frame.ready)
                    || stream.negotiated.as_ref().is_some_and(|queue| {
                        queue
                            .ready
                            .iter()
                            .any(|unit| unit.flags & VIDEO_UNIT_KEYFRAME != 0)
                    })
            })
            .ok_or(MEDIA_CLOSED)
    }

    pub fn eos_queued(&self, id: u64) -> bool {
        self.streams.get(&id).is_some_and(|stream| {
            stream
                .negotiated
                .as_ref()
                .map_or(stream.eos_queued, |q| q.eos_queued)
        })
    }

    pub fn eos_accepted(&self, id: u64) -> bool {
        self.streams.get(&id).is_some_and(|stream| {
            stream
                .negotiated
                .as_ref()
                .map_or(stream.eos_accepted, |q| q.eos_accepted)
        })
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

    /// Replace a provisional start with the capture provider's earlier start
    /// of the presentation actually submitted for `sequence`. Only an entry
    /// recorded in the current generation and not yet frozen by encoded output
    /// changes, and only within the host latency sanity window; otherwise the
    /// provisional measurement stays. Returns whether the provider start applied.
    pub fn adopt_processing_start(&mut self, id: u64, sequence: u64, provider_us: u64) -> bool {
        let generation = self.processing_generation.load(Ordering::Relaxed);
        let Some(stream) = self.streams.get_mut(&id) else {
            return false;
        };
        if stream.frozen_processing_starts.contains_key(&sequence) {
            return false;
        }
        let Some((started, recorded_generation)) = stream.processing_starts.get_mut(&sequence)
        else {
            return false;
        };
        if generation == u64::MAX || *recorded_generation != generation {
            return false;
        }
        let selected =
            amaterasu_module_sdk::capture::select_processing_start(Some(provider_us), *started);
        let adopted = selected != *started;
        *started = selected;
        adopted
    }

    #[cfg(test)]
    pub fn processing_start(&self, id: u64, sequence: u64) -> Option<u64> {
        self.streams
            .get(&id)?
            .processing_starts
            .get(&sequence)
            .map(|&(started, _)| started)
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
        if stream.negotiated.is_some() {
            return STATUS_INVALID_ARGUMENT;
        }
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
        if stream
            .startup_frame
            .as_ref()
            .is_some_and(|frame| !frame.accepts(packet))
        {
            return STATUS_INVALID_ARGUMENT;
        }
        // Before PLAY there is no consumer to free fragment slots. Coalesce
        // only a contiguous unfinished startup AU into its existing queue tail
        // when needed, retaining the same four slots and total byte bound.
        let merge_startup_tail = stream.packets.len() >= 4
            && stream
                .startup_frame
                .as_ref()
                .is_some_and(|frame| !frame.ready)
            && packet.flags != FLAG_END_OF_STREAM
            && stream.packets.back().is_some_and(|tail| {
                tail.codec == packet.codec
                    && tail.sequence == packet.sequence
                    && tail.timestamp_us == packet.timestamp_us
                    && tail.flags & (FLAG_END_OF_ACCESS_UNIT | FLAG_END_OF_STREAM) == 0
            });
        if (stream.packets.len() >= 4 && !merge_startup_tail)
            || stream.bytes.saturating_add(packet.len) > MAX_PACKET_BYTES
        {
            return MEDIA_BUSY;
        }
        // SAFETY: provider retains packet for the whole sink invocation.
        let bytes = match unsafe { packet.bytes() } {
            Ok(bytes) => bytes,
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
        if let Some(frame) = &mut stream.startup_frame {
            frame.observe(packet);
        }
        if merge_startup_tail {
            let tail = stream.packets.back_mut().expect("verified startup tail");
            tail.bytes.extend_from_slice(bytes);
            tail.flags |= packet.flags;
        } else {
            stream.packets.push_back(Packet {
                queued_at: Instant::now(),
                bytes: bytes.to_vec(),
                codec: packet.codec,
                flags: packet.flags,
                timestamp_us: packet.timestamp_us,
                sequence: packet.sequence,
                processing_started_host_us,
            });
        }
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
        if stream.negotiated.is_some() {
            return STATUS_INVALID_ARGUMENT;
        }
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

    #[test]
    fn startup_frame_requires_a_complete_keyframe_and_survives_fragment_pressure() {
        let mut queues = Queues::default();
        queues.open(1, "alice", 0);
        queues.enable_startup_readiness(1);
        assert_eq!(queues.startup_frame_ready("alice", 1), Ok(false));
        assert_eq!(queues.startup_frame_ready("bob", 1), Err(MEDIA_CLOSED));
        let byte = [3];
        for fragment in 0..9 {
            let packet = EncodedPacket {
                codec: CODEC_H264,
                flags: if fragment == 0 {
                    FLAG_KEYFRAME
                } else if fragment == 8 {
                    FLAG_END_OF_ACCESS_UNIT
                } else {
                    0
                },
                data: byte.as_ptr(),
                len: 1,
                timestamp_us: 7,
                sequence: 5,
            };
            // SAFETY: byte remains live through each synchronous copy.
            assert_eq!(unsafe { queues.push(1, &packet) }, STATUS_OK);
            assert_eq!(queues.startup_frame_ready("alice", 1), Ok(fragment == 8));
            assert!(queues.streams[&1].packets.len() <= 4);
        }
        assert_eq!(queues.streams[&1].bytes, 9);
        assert_eq!(queues.streams[&1].packets.len(), 4);
        assert_eq!(
            queues.streams[&1].packets.back().unwrap().flags,
            FLAG_END_OF_ACCESS_UNIT
        );
        unsafe extern "C" fn accept_startup(_: *mut c_void, _: *const EncodedPacket) -> Status {
            STATUS_OK
        }
        let sink = PacketSink {
            context: std::ptr::null_mut(),
            write: accept_startup,
        };
        for _ in 0..4 {
            // SAFETY: accept only borrows the queue-owned packet until return.
            assert_eq!(unsafe { queues.receive("alice", 1, sink) }, STATUS_OK);
        }
        assert_eq!(queues.startup_frame_ready("alice", 1), Ok(true));
        queues.close(1);
        queues.open(1, "alice", 0);
        queues.enable_startup_readiness(1);
        assert_eq!(queues.startup_frame_ready("alice", 1), Ok(false));
    }

    #[test]
    fn startup_frame_rejects_broken_boundaries_and_never_accepts_eos_as_readiness() {
        let byte = [1];
        let first = EncodedPacket {
            codec: CODEC_H264,
            flags: FLAG_KEYFRAME,
            data: byte.as_ptr(),
            len: 1,
            timestamp_us: 8,
            sequence: 4,
        };
        for changed in [
            EncodedPacket {
                sequence: 5,
                flags: FLAG_END_OF_ACCESS_UNIT,
                ..first
            },
            EncodedPacket {
                timestamp_us: 9,
                flags: FLAG_END_OF_ACCESS_UNIT,
                ..first
            },
            EncodedPacket {
                codec: amaterasu_media::CODEC_HEVC,
                flags: FLAG_END_OF_ACCESS_UNIT,
                ..first
            },
            EncodedPacket {
                flags: FLAG_END_OF_STREAM,
                data: std::ptr::null(),
                len: 0,
                ..first
            },
        ] {
            let mut queues = Queues::default();
            queues.open(1, "alice", 0);
            queues.enable_startup_readiness(1);
            // SAFETY: byte is a live immutable allocation for both packets.
            assert_eq!(unsafe { queues.push(1, &first) }, STATUS_OK);
            assert_eq!(unsafe { queues.push(1, &changed) }, STATUS_INVALID_ARGUMENT);
            assert_eq!(queues.startup_frame_ready("alice", 1), Ok(false));
            assert_eq!(queues.streams[&1].bytes, 1);
        }
        let mut queues = Queues::default();
        queues.open(1, "alice", 0);
        queues.enable_startup_readiness(1);
        let non_keyframe = EncodedPacket {
            flags: FLAG_END_OF_ACCESS_UNIT,
            ..first
        };
        assert_eq!(unsafe { queues.push(1, &non_keyframe) }, STATUS_OK);
        assert_eq!(queues.startup_frame_ready("alice", 1), Ok(false));
        let eos = EncodedPacket {
            flags: FLAG_END_OF_STREAM,
            data: std::ptr::null(),
            len: 0,
            ..first
        };
        assert_eq!(unsafe { queues.push(1, &eos) }, STATUS_OK);
        assert_eq!(queues.startup_frame_ready("alice", 1), Ok(false));
    }

    #[test]
    fn startup_fragment_aggregation_keeps_byte_and_fragment_limits() {
        let mut queues = Queues::default();
        queues.open(1, "alice", 0);
        queues.enable_startup_readiness(1);
        let byte = [1];
        let packet = EncodedPacket {
            codec: CODEC_H264,
            flags: FLAG_KEYFRAME,
            data: byte.as_ptr(),
            len: 1,
            timestamp_us: 8,
            sequence: 4,
        };
        for _ in 0..1024 {
            assert_eq!(unsafe { queues.push(1, &packet) }, STATUS_OK);
        }
        assert_eq!(unsafe { queues.push(1, &packet) }, STATUS_INVALID_ARGUMENT);
        assert_eq!(queues.streams[&1].packets.len(), 4);
        assert_eq!(queues.startup_frame_ready("alice", 1), Ok(false));
        let bytes = vec![1; MAX_PACKET_BYTES - 3];
        queues.close(1);
        queues.open(1, "alice", 0);
        queues.enable_startup_readiness(1);
        let large = EncodedPacket {
            data: bytes.as_ptr(),
            len: bytes.len(),
            ..packet
        };
        assert_eq!(unsafe { queues.push(1, &large) }, STATUS_OK);
        for _ in 0..3 {
            assert_eq!(unsafe { queues.push(1, &packet) }, STATUS_OK);
        }
        let last = EncodedPacket {
            flags: FLAG_END_OF_ACCESS_UNIT,
            ..packet
        };
        assert_eq!(unsafe { queues.push(1, &last) }, MEDIA_BUSY);
        assert_eq!(queues.streams[&1].bytes, MAX_PACKET_BYTES);
        assert_eq!(queues.startup_frame_ready("alice", 1), Ok(false));
    }

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
    fn provider_start_replaces_only_a_live_provisional_start_inside_the_window() {
        let mut queues = Queues::default();
        queues.open(1, "alice", 1000);
        // No provisional start: the provider cannot invent one.
        assert!(!queues.adopt_processing_start(1, 7, 4000));
        queues.start_processing(1, 7, 9000);
        // A start from the future is rejected; the provisional value stays.
        assert!(!queues.adopt_processing_start(1, 7, 9001));
        assert!(queues.adopt_processing_start(1, 7, 4000));
        assert_eq!(queues.streams[&1].processing_starts[&7].0, 4000);
        // A stale generation is never upgraded into a precise value.
        queues.start_processing(1, 8, 9000);
        queues
            .processing_generation()
            .fetch_add(1, Ordering::Relaxed);
        assert!(!queues.adopt_processing_start(1, 8, 4000));
        // Output already frozen with the provisional start keeps it.
        queues.start_processing(1, 9, 9000);
        let data = [1u8];
        let packet = EncodedPacket {
            codec: CODEC_H264,
            flags: 0,
            data: data.as_ptr(),
            len: 1,
            timestamp_us: 99,
            sequence: 9,
        };
        // SAFETY: packet bytes remain live through this synchronous copy.
        assert_eq!(unsafe { queues.push(1, &packet) }, STATUS_OK);
        assert!(!queues.adopt_processing_start(1, 9, 4000));
        assert_eq!(queues.streams[&1].frozen_processing_starts[&9], 9000);
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
    fn submitted_startup_requires_a_complete_keyframe_from_the_owned_queue() {
        let mut queues = Queues::default();
        queues.open(1, "alice", 1000);
        let format = EncoderFormatId {
            namespace_id: 1,
            value: 2,
            schema: 3,
        };
        queues
            .configure_units(
                1,
                UnitSelection {
                    stream: 1,
                    generation: 1,
                    offer_id: 1,
                    source_id: 1,
                    config_generation: 1,
                    codec: format,
                    framing: format,
                    clock_epoch: [1; 16],
                },
            )
            .unwrap();
        queues.enable_startup_readiness(1);
        let bytes = [42];
        let mut chunk = EncoderOutputChunk {
            session_id: 1,
            size: std::mem::size_of::<EncoderOutputChunk>() as u32,
            codec: format,
            framing: format,
            source_id: 1,
            config_generation: 1,
            access_unit_id: 1,
            submission_id: 1,
            timestamp_us: 1,
            flags: ENCODER_CHUNK_ACCESS_UNIT_START | ENCODER_CHUNK_KEYFRAME,
            bytes: BorrowedBytes::new(&bytes),
        };
        // SAFETY: the descriptor and bytes remain live throughout the synchronous copies.
        assert_eq!(unsafe { queues.push_unit_chunk(1, &chunk) }, STATUS_OK);
        assert_eq!(queues.startup_frame_ready("alice", 1), Ok(false));
        chunk.flags = ENCODER_CHUNK_ACCESS_UNIT_END | ENCODER_CHUNK_KEYFRAME;
        assert_eq!(unsafe { queues.push_unit_chunk(1, &chunk) }, STATUS_OK);
        assert_eq!(queues.startup_frame_ready("alice", 1), Ok(true));
        assert_eq!(queues.startup_frame_ready("bob", 1), Err(MEDIA_CLOSED));
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
