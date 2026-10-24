use crate::{
    layout,
    model::{AmdAv1LatencyMode, Configuration, EncoderIdentity, Packet},
    worker::Worker,
};
use amaterasu_abi::{STATUS_INVALID_ARGUMENT, STATUS_OK, Status};
use amaterasu_media::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::time::Instant;

use crate::worker::{Metrics as WorkerMetrics, ReceiveResult, SubmitResult};

pub trait Backend: Send {
    fn submit(
        &mut self,
        bytes: Vec<u8>,
        timestamp: i64,
        force: bool,
    ) -> Result<SubmitResult, &'static str>;
    fn receive(&mut self) -> Result<ReceiveResult, &'static str>;
    fn drain(&mut self) -> Result<bool, &'static str>;
    fn metrics(&mut self) -> Result<Option<WorkerMetrics>, &'static str> {
        Ok(None)
    }
}

impl Backend for Worker {
    fn submit(
        &mut self,
        bytes: Vec<u8>,
        timestamp: i64,
        force: bool,
    ) -> Result<SubmitResult, &'static str> {
        self.submit(bytes, timestamp, force)
    }
    fn receive(&mut self) -> Result<ReceiveResult, &'static str> {
        self.receive()
    }
    fn drain(&mut self) -> Result<bool, &'static str> {
        self.drain()
    }
    fn metrics(&mut self) -> Result<Option<WorkerMetrics>, &'static str> {
        self.metrics().map(Some)
    }
}

struct Metadata {
    timestamp: i64,
    sequence: u64,
    force_keyframe: bool,
}

pub struct Session {
    packet_boundaries: bool,
    owner: String,
    config: Configuration,
    backend: Box<dyn Backend>,
    metadata: VecDeque<Metadata>,
    last_input: Option<(i64, u64)>,
    pending: Option<Packet>,
    draining: bool,
    eos_sent: bool,
    failed: bool,
    color_conversion_total_us: u64,
    color_conversion_max_us: u64,
    color_conversion_frames: u64,
}

struct OpenRecord {
    packet_boundaries: bool,
    id: u64,
    encoder: EncoderIdentity,
    configuration: Configuration,
}

pub struct Sessions {
    pub enabled: bool,
    next_id: u64,
    operations: BTreeMap<(String, String), OpenRecord>,
    values: BTreeMap<u64, Session>,
}

impl Sessions {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            next_id: 0,
            operations: BTreeMap::new(),
            values: BTreeMap::new(),
        }
    }
    pub fn open(
        &mut self,
        owner: &str,
        encoder: EncoderIdentity,
        configuration: Configuration,
        operation: Option<String>,
        packet_boundaries: bool,
    ) -> Result<Value, &'static str> {
        if configuration.nvenc_h264_cavlc {
            return Err("nvenc_h264_cavlc_unsupported");
        }
        if configuration.nvenc_spatial_aq {
            return Err("nvenc_spatial_aq_unsupported");
        }
        if configuration.amd_av1_latency_mode != AmdAv1LatencyMode::Auto {
            return Err("amd_av1_latency_mode_unsupported");
        }
        if let Some(operation) = &operation {
            validate_operation(operation)?;
            if let Some(record) = self.operations.get(&(owner.into(), operation.clone())) {
                if record.configuration != configuration
                    || record.encoder != encoder
                    || record.packet_boundaries != packet_boundaries
                {
                    return Err("operation_conflict");
                }
                if !self.values.contains_key(&record.id) {
                    return Err("session_closed");
                }
                return Ok(open_reply(record.id, &configuration, packet_boundaries));
            }
            if self.operations.len() >= 128 {
                return Err("operation_limit");
            }
        }
        self.validate_open(owner, &encoder, &configuration)?;
        let backend = Worker::open(encoder.clone(), configuration.clone())?;
        let response = self.insert(
            owner,
            configuration.clone(),
            Box::new(backend),
            packet_boundaries,
        )?;
        if let Some(operation) = operation {
            self.operations.insert(
                (owner.into(), operation),
                OpenRecord {
                    packet_boundaries,
                    id: self.next_id,
                    encoder,
                    configuration,
                },
            );
        }
        Ok(response)
    }
    fn validate_open(
        &self,
        owner: &str,
        encoder: &EncoderIdentity,
        config: &Configuration,
    ) -> Result<(), &'static str> {
        if !self.enabled {
            return Err("disabled");
        }
        config.validate()?;
        encoder.validate()?;
        if self.values.len() >= 8
            || self
                .values
                .values()
                .filter(|session| session.owner == owner)
                .count()
                >= 2
        {
            return Err("session_limit");
        }
        Ok(())
    }
    fn insert(
        &mut self,
        owner: &str,
        configuration: Configuration,
        backend: Box<dyn Backend>,
        packet_boundaries: bool,
    ) -> Result<Value, &'static str> {
        self.next_id = self.next_id.checked_add(1).ok_or("session_limit")?;
        let packet_boundaries = packet_boundaries || configuration.codec == VideoCodec::Hevc;
        let response = open_reply(self.next_id, &configuration, packet_boundaries);
        self.values.insert(
            self.next_id,
            Session {
                packet_boundaries,
                owner: owner.into(),
                config: configuration,
                backend,
                metadata: VecDeque::new(),
                last_input: None,
                pending: None,
                draining: false,
                eos_sent: false,
                failed: false,
                color_conversion_total_us: 0,
                color_conversion_max_us: 0,
                color_conversion_frames: 0,
            },
        );
        Ok(response)
    }
    fn owned(&mut self, owner: &str, id: u64) -> Result<&mut Session, &'static str> {
        self.values
            .get_mut(&id)
            .filter(|session| session.owner == owner)
            .ok_or("session_not_found")
    }
    pub fn status(&mut self, owner: &str, id: u64) -> Result<Value, &'static str> {
        let session = self.owned(owner, id)?;
        let encoder_metrics = match session.backend.metrics() {
            Ok(metrics) => metrics,
            Err(_) => {
                session.failed = true;
                None
            }
        };
        let state = if session.failed {
            "failed"
        } else if session.draining {
            "draining"
        } else {
            "open"
        };
        Ok(
            json!({"ok":true,"session_id":id,"state":state,"configuration":session.config,"packet_boundaries":session.packet_boundaries,"supports_packet_boundaries":true,"timing":{"color_conversion_frames":session.color_conversion_frames,"color_conversion_total_us":session.color_conversion_total_us,"color_conversion_max_us":session.color_conversion_max_us},"encoder_timing":encoder_metrics}),
        )
    }
    pub fn close(&mut self, owner: &str, id: u64) -> Result<Value, &'static str> {
        self.owned(owner, id)?;
        self.values.remove(&id);
        Ok(json!({"ok":true,"session_id":id,"state":"closed"}))
    }
    pub fn lookup(&self, owner: &str, operation: &str) -> Result<Value, &'static str> {
        validate_operation(operation)?;
        let record = self
            .operations
            .get(&(owner.into(), operation.into()))
            .ok_or("session_not_found")?;
        if !self.values.contains_key(&record.id) {
            return Err("session_not_found");
        }
        Ok(open_reply(
            record.id,
            &record.configuration,
            record.packet_boundaries,
        ))
    }
    pub fn latest_id(&self) -> u64 {
        self.next_id
    }
    pub fn clear(&mut self) {
        self.values.clear();
    }
    /// # Safety
    /// Frame allocation is borrowed for this call and contains its declared bytes.
    pub unsafe fn submit(&mut self, owner: &str, id: u64, frame: &CpuFrame, force: u32) -> Status {
        let Ok(session) = self.owned(owner, id) else {
            return MEDIA_CLOSED;
        };
        if session.failed {
            return MEDIA_BACKEND_ERROR;
        }
        if session.draining {
            return MEDIA_CLOSED;
        }
        if session.metadata.len() >= 16 {
            return MEDIA_BUSY;
        }
        if frame.width != session.config.width || frame.height != session.config.height || force > 1
        {
            return STATUS_INVALID_ARGUMENT;
        }
        let Some(timestamp) = frame
            .timestamp_us
            .checked_mul(10)
            .and_then(|time| i64::try_from(time).ok())
        else {
            return STATUS_INVALID_ARGUMENT;
        };
        if session
            .last_input
            .is_some_and(|(last_timestamp, last_sequence)| {
                timestamp <= last_timestamp || frame.sequence <= last_sequence
            })
        {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: native caller guarantees the allocation; validation precedes read.
        let bytes = match unsafe { frame.bytes() } {
            Ok(bytes) => bytes,
            Err(status) => return status,
        };
        if !layout::input_color_supported(frame.format, session.config.color_mode) {
            return STATUS_INVALID_ARGUMENT;
        }
        let conversion_started = Instant::now();
        let bytes = match frame.format {
            PIXEL_BGRA8 => layout::bgra(
                bytes,
                frame.width as usize,
                frame.height as usize,
                frame.stride as usize,
                session.config.color_mode,
            ),
            PIXEL_NV12 => layout::nv12(
                bytes,
                frame.width as usize,
                frame.height as usize,
                frame.stride as usize,
            ),
            _ => return MEDIA_UNSUPPORTED,
        };
        let bytes = match bytes {
            Ok(bytes) => bytes,
            Err(_) => return STATUS_INVALID_ARGUMENT,
        };
        if frame.format == PIXEL_BGRA8 {
            let conversion_us = conversion_started
                .elapsed()
                .as_micros()
                .min(u128::from(u64::MAX)) as u64;
            session.color_conversion_frames = session.color_conversion_frames.saturating_add(1);
            session.color_conversion_total_us = session
                .color_conversion_total_us
                .saturating_add(conversion_us);
            session.color_conversion_max_us = session.color_conversion_max_us.max(conversion_us);
        }
        match session.backend.submit(bytes, timestamp, force != 0) {
            Ok(SubmitResult::Accepted) => {}
            Ok(SubmitResult::Busy) => return MEDIA_BUSY,
            Err(_) => {
                session.failed = true;
                return MEDIA_BACKEND_ERROR;
            }
        }
        session.metadata.push_back(Metadata {
            timestamp,
            sequence: frame.sequence,
            force_keyframe: force != 0,
        });
        session.last_input = Some((timestamp, frame.sequence));
        STATUS_OK
    }
    pub fn drain(&mut self, owner: &str, id: u64) -> Status {
        let Ok(session) = self.owned(owner, id) else {
            return MEDIA_CLOSED;
        };
        if session.failed {
            return MEDIA_BACKEND_ERROR;
        }
        if !session.draining {
            match session.backend.drain() {
                Ok(true) => {}
                Ok(false) => return MEDIA_BUSY,
                Err(_) => {
                    session.failed = true;
                    return MEDIA_BACKEND_ERROR;
                }
            }
            session.draining = true;
        }
        STATUS_OK
    }
    /// # Safety
    /// Sink callback/context must remain live and obey the borrowed packet ABI.
    pub unsafe fn receive(&mut self, owner: &str, id: u64, sink: PacketSink) -> Status {
        let Ok(session) = self.owned(owner, id) else {
            return MEDIA_CLOSED;
        };
        if session.failed {
            return MEDIA_BACKEND_ERROR;
        }
        if session.eos_sent {
            return MEDIA_PENDING;
        }
        if session.pending.is_none() {
            match session.backend.receive() {
                Ok(ReceiveResult::Ready(packet)) => session.pending = packet,
                Ok(ReceiveResult::Pending) => return MEDIA_PENDING,
                Err(_) => {
                    session.failed = true;
                    return MEDIA_BACKEND_ERROR;
                }
            }
        }
        let Some(packet) = session.pending.as_ref() else {
            if !session.draining || session.eos_sent {
                return MEDIA_PENDING;
            }
            let packet = EncodedPacket {
                codec: session.config.codec.abi_id(),
                flags: FLAG_END_OF_STREAM,
                data: std::ptr::null(),
                len: 0,
                timestamp_us: 0,
                sequence: 0,
            };
            // SAFETY: empty EOS has no allocation and callback is live by contract.
            let status = unsafe { sink.send(&packet) };
            if status == STATUS_OK {
                session.eos_sent = true;
            }
            return status;
        };
        let Some(metadata) = session
            .metadata
            .iter()
            .find(|metadata| metadata.timestamp == packet.timestamp_100ns)
        else {
            session.failed = true;
            return MEDIA_BACKEND_ERROR;
        };
        if metadata.force_keyframe && !packet.keyframe {
            session.failed = true;
            return MEDIA_BACKEND_ERROR;
        }
        let encoded = EncodedPacket {
            codec: session.config.codec.abi_id(),
            // Encoder packets are whole access units. HEVC always marks this
            // packet as the final fragment, even without H.264 boundary opt-in.
            flags: if session.packet_boundaries {
                FLAG_END_OF_ACCESS_UNIT
            } else {
                0
            } | if packet.keyframe { FLAG_KEYFRAME } else { 0 },
            data: packet.bytes.as_ptr(),
            len: packet.bytes.len(),
            timestamp_us: (packet.timestamp_100ns / 10) as u64,
            sequence: metadata.sequence,
        };
        // SAFETY: pending owns the packet for the callback's duration. It remains
        // retained on rejection; native callbacks must not recursively mutate us.
        let status = unsafe { sink.send(&encoded) };
        if status == STATUS_OK {
            let timestamp = packet.timestamp_100ns;
            session.pending = None;
            while session
                .metadata
                .front()
                .is_some_and(|metadata| metadata.timestamp <= timestamp)
            {
                session.metadata.pop_front();
            }
        }
        status
    }
}

fn validate_operation(operation: &str) -> Result<(), &'static str> {
    if operation.is_empty()
        || operation.len() > 128
        || !operation.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err("invalid_operation_id");
    }
    Ok(())
}

fn open_reply(id: u64, config: &Configuration, packet_boundaries: bool) -> Value {
    let packet_boundaries = packet_boundaries || config.codec == VideoCodec::Hevc;
    json!({"ok":true,"session_id":id,"configuration":config,"codec":match config.codec { VideoCodec::H264 => "h264", VideoCodec::Hevc => "hevc" },"max_reference_frames":config.max_reference_frames,"packet_boundaries":packet_boundaries,"supports_packet_boundaries":true})
}

#[cfg(test)]
mod tests;
