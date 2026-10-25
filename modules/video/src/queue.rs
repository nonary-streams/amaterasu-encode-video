//! Bounded encoded packet ownership shared with native consumer bindings.
use amaterasu_media::{
    EncodedPacket, FLAG_END_OF_STREAM, MAX_PACKET_BYTES, MEDIA_BUSY, MEDIA_CLOSED, MEDIA_PENDING,
    PacketSink,
};
use amaterasu_module_sdk::abi::{STATUS_OK, Status};
use std::collections::{BTreeMap, VecDeque};

struct Packet {
    bytes: Vec<u8>,
    codec: u32,
    flags: u32,
    timestamp_us: u64,
    sequence: u64,
}

struct Stream {
    owner: String,
    /// Host-clock microseconds at the stream's timestamp origin. Packets
    /// leave the queue stamped on the shared host clock (see
    /// `amaterasu_media::host_monotonic_micros`) so consumers can measure
    /// host latency; inside video they stay relative to the stream's open.
    clock_origin_us: u64,
    geometry: Option<amaterasu_media::CaptureGeometry>,
    packets: VecDeque<Packet>,
    bytes: usize,
    eos_queued: bool,
    eos_accepted: bool,
}

#[derive(Default)]
pub struct Queues {
    streams: BTreeMap<u64, Stream>,
}

impl Queues {
    pub fn clear(&mut self) {
        self.streams.clear();
    }

    pub fn open(&mut self, id: u64, owner: &str, clock_origin_us: u64) {
        self.streams.insert(
            id,
            Stream {
                owner: owner.into(),
                clock_origin_us,
                geometry: None,
                packets: VecDeque::new(),
                bytes: 0,
                eos_queued: false,
                eos_accepted: false,
            },
        );
    }

    pub fn set_geometry(&mut self, id: u64, geometry: Option<amaterasu_media::CaptureGeometry>) {
        if let Some(stream) = self.streams.get_mut(&id) {
            stream.geometry = geometry;
        }
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
        stream.packets.push_back(Packet {
            bytes,
            codec: packet.codec,
            flags: packet.flags,
            timestamp_us: packet.timestamp_us,
            sequence: packet.sequence,
        });
        STATUS_OK
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
            timestamp_us: stream.clock_origin_us.saturating_add(packet.timestamp_us),
            sequence: packet.sequence,
        };
        // SAFETY: queue owns packet during callback; rejection retains it.
        let status = unsafe { sink.send(&borrowed) };
        if status == STATUS_OK {
            stream.eos_accepted = packet.flags == FLAG_END_OF_STREAM;
            stream.bytes -= packet.bytes.len();
            stream.packets.pop_front();
        }
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amaterasu_media::CODEC_H264;
    use std::ffi::c_void;
    unsafe extern "C" fn reject(_: *mut c_void, _: *const EncodedPacket) -> Status {
        MEDIA_BUSY
    }
    unsafe extern "C" fn accept(_: *mut c_void, packet: *const EncodedPacket) -> Status {
        // SAFETY: queue borrows this packet for the callback.
        assert_eq!(unsafe { (*packet).bytes() }.unwrap(), &[1, 2, 3]);
        STATUS_OK
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
