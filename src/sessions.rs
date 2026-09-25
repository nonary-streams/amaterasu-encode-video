use crate::{
    layout,
    model::{AmdAv1LatencyMode, Configuration, EncoderIdentity, Packet},
    worker::Worker,
};
use amaterasu_abi::{STATUS_INVALID_ARGUMENT, STATUS_OK, Status};
use amaterasu_media::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};

pub trait Backend: Send {
    fn submit(&mut self, bytes: Vec<u8>, timestamp: i64, force: bool) -> Result<(), &'static str>;
    fn receive(&mut self) -> Result<Option<Packet>, &'static str>;
    fn drain(&mut self) -> Result<(), &'static str>;
}

impl Backend for Worker {
    fn submit(&mut self, bytes: Vec<u8>, timestamp: i64, force: bool) -> Result<(), &'static str> {
        self.submit(bytes, timestamp, force)
    }
    fn receive(&mut self) -> Result<Option<Packet>, &'static str> {
        self.receive()
    }
    fn drain(&mut self) -> Result<(), &'static str> {
        self.drain()
    }
}

struct Metadata {
    timestamp: i64,
    sequence: u64,
}

pub struct Session {
    packet_boundaries: bool,
    owner: String,
    config: Configuration,
    backend: Box<dyn Backend>,
    metadata: VecDeque<Metadata>,
    pending: Option<Packet>,
    draining: bool,
    eos_sent: bool,
    failed: bool,
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
        let response = open_reply(self.next_id, &configuration, packet_boundaries);
        self.values.insert(
            self.next_id,
            Session {
                packet_boundaries,
                owner: owner.into(),
                config: configuration,
                backend,
                metadata: VecDeque::new(),
                pending: None,
                draining: false,
                eos_sent: false,
                failed: false,
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
        let state = if session.failed {
            "failed"
        } else if session.draining {
            "draining"
        } else {
            "open"
        };
        Ok(
            json!({"ok":true,"session_id":id,"state":state,"configuration":session.config,"packet_boundaries":session.packet_boundaries,"supports_packet_boundaries":true}),
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
            .metadata
            .back()
            .is_some_and(|last| timestamp <= last.timestamp || frame.sequence <= last.sequence)
        {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: native caller guarantees the allocation; validation precedes read.
        let bytes = match unsafe { frame.bytes() } {
            Ok(bytes) => bytes,
            Err(status) => return status,
        };
        let convert = match frame.format {
            PIXEL_BGRA8 => layout::bgra,
            PIXEL_NV12 => layout::nv12,
            _ => return MEDIA_UNSUPPORTED,
        };
        let bytes = match convert(
            bytes,
            frame.width as usize,
            frame.height as usize,
            frame.stride as usize,
        ) {
            Ok(bytes) => bytes,
            Err(_) => return STATUS_INVALID_ARGUMENT,
        };
        if session
            .backend
            .submit(bytes, timestamp, force != 0)
            .is_err()
        {
            session.failed = true;
            return MEDIA_BACKEND_ERROR;
        }
        session.metadata.push_back(Metadata {
            timestamp,
            sequence: frame.sequence,
        });
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
            if session.backend.drain().is_err() {
                session.failed = true;
                return MEDIA_BACKEND_ERROR;
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
                Ok(packet) => session.pending = packet,
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
                codec: CODEC_H264,
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
        let encoded = EncodedPacket {
            codec: CODEC_H264,
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
                .is_some_and(|metadata| metadata.timestamp < timestamp)
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
    json!({"ok":true,"session_id":id,"configuration":config,"codec":"h264","max_reference_frames":config.max_reference_frames,"packet_boundaries":packet_boundaries,"supports_packet_boundaries":true})
}

#[cfg(test)]
mod tests;
