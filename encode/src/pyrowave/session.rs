//! Caller-owned PyroWave sessions. No capture, transport or socket policy lives here.
use super::library::{self, CpuBuffer, Runtime};
use amaterasu_media::*;
use amaterasu_module_sdk::{
    abi::*,
    encode::{self, EncoderProvider, FormatId, OutputQueue, SessionMode},
    media::BorrowedResourceFrame,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

// Codec syntax is tied to the pinned upstream revision; the native bitstream
// has no embedded version. IDs are negotiated data, never a core codec enum.
pub const NAMESPACE: u64 = 0x5059_524f_5741_5645;
pub const CODEC: FormatId = FormatId {
    namespace_id: NAMESPACE,
    value: 1,
    schema: 0x186f_0393,
};
pub const RECORDS: FormatId = FormatId {
    namespace_id: NAMESPACE,
    value: 2,
    schema: 1,
};
pub const OPTIONS: FormatId = FormatId {
    namespace_id: NAMESPACE,
    value: 3,
    schema: 1,
};
pub const CODEC_HDR10: FormatId = FormatId {
    namespace_id: NAMESPACE,
    value: 4,
    schema: 0x186f_0393,
};
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    #[default]
    Sdr8,
    Hdr10,
}
impl Profile {
    fn is_sdr(&self) -> bool {
        *self == Self::Sdr8
    }
    fn codec(self) -> FormatId {
        if self == Self::Hdr10 {
            CODEC_HDR10
        } else {
            CODEC
        }
    }
    pub fn hdr(self) -> bool {
        self == Self::Hdr10
    }
}
const MAX_SESSIONS: usize = 8;
const MAX_OPERATIONS: usize = 128;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    #[serde(default, skip_serializing_if = "Profile::is_sdr")]
    pub profile: Profile,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u64,
    pub clock_epoch: [u8; 16],
    pub clock_origin_us: u64,
}
impl Configuration {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(16..=3840).contains(&self.width)
            || !(16..=2160).contains(&self.height)
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
            || !(1..=240).contains(&self.fps)
            || !(100_000..=4_000_000_000).contains(&self.bitrate)
            || self.clock_epoch == [0; 16]
            || !(1024..=8 * 1024 * 1024).contains(&self.budget())
        {
            return Err("invalid_pyrowave_configuration");
        }
        Ok(())
    }
    pub fn budget(&self) -> usize {
        (self.bitrate / (8 * u64::from(self.fps.max(1)))) as usize
    }
}
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Enumerate,
    Probe {
        configuration: Configuration,
    },
    Open {
        operation_id: String,
        configuration: Configuration,
    },
    Lookup {
        operation_id: String,
    },
    Close {
        session_id: u64,
    },
}
struct OperationRecord {
    owner: String,
    operation: String,
    configuration: Configuration,
    id: u64,
}
pub(super) struct Session {
    owner: String,
    configuration: Configuration,
    encoder: Option<library::Encoder>,
    runtime: Arc<Runtime>,
    queue: OutputQueue,
    source: Option<u64>,
    pub selection: Option<Selection>,
    last_sequence: u64,
    last_timestamp: Option<u64>,
    draining: bool,
}
#[derive(Clone, Copy)]
pub(super) struct Selection {
    pub generation: u64,
    pub id: u64,
    pub format: u32,
    pub dmabuf: Option<DmaBufResourceCapabilities>,
}
pub(super) struct State {
    runtime: Arc<Runtime>,
    sessions: BTreeMap<u64, Session>,
    operations: Vec<OperationRecord>,
    next_id: u64,
}
pub(super) type Shared = Arc<Mutex<State>>;
impl State {
    pub fn new(runtime: Arc<Runtime>) -> Shared {
        Arc::new(Mutex::new(Self {
            runtime,
            sessions: BTreeMap::new(),
            operations: Vec::new(),
            next_id: 1,
        }))
    }
    pub fn session(&self, owner: &str, id: u64) -> Result<&Session, Status> {
        self.sessions
            .get(&id)
            .filter(|s| s.owner == owner)
            .ok_or(MEDIA_CLOSED)
    }
    pub fn session_mut(&mut self, owner: &str, id: u64) -> Result<&mut Session, Status> {
        self.sessions
            .get_mut(&id)
            .filter(|s| s.owner == owner)
            .ok_or(MEDIA_CLOSED)
    }
    pub fn close(&mut self, owner: &str, id: u64) {
        if self.session(owner, id).is_ok() {
            let session = self.sessions.remove(&id).expect("authorized session");
            // Session drop retires notifications and waits for native GPU work.
            drop(session);
        }
    }
    pub fn close_owner(&mut self, owner: &str) {
        let ids: Vec<_> = self
            .sessions
            .iter()
            .filter_map(|(&id, s)| (s.owner == owner).then_some(id))
            .collect();
        for id in ids {
            self.close(owner, id);
        }
    }
    pub fn control(
        &mut self,
        owner: &str,
        payload: Value,
    ) -> Result<(Value, Option<u64>), &'static str> {
        let operation = serde_json::from_value(payload).map_err(|_| "invalid_request")?;
        match operation {
            Operation::Enumerate => Ok((
                json!({"ok":true,"gpu_only":self.runtime.gpu_only(),"profiles":self.profiles()}),
                None,
            )),
            Operation::Probe { configuration } => {
                configuration.validate()?;
                probe(&self.runtime, &configuration)?;
                Ok((
                    json!({"ok":true,"usable":true,"settled":true,"configuration":configuration,
                    "codec":format_json(configuration.profile.codec()),"framing":format_json(RECORDS),
                    "probe_scope":if self.runtime.gpu_only(){"gpu_encode"}else{"cpu_input_encode"},
                    "capture_import_verified":false}),
                    None,
                ))
            }
            Operation::Open {
                operation_id,
                configuration,
            } => self.open(owner, operation_id, configuration),
            Operation::Lookup { operation_id } => {
                validate_operation(&operation_id)?;
                let record = self
                    .operations
                    .iter()
                    .find(|r| r.owner == owner && r.operation == operation_id)
                    .ok_or("session_not_found")?;
                Ok((
                    self.sessions
                        .get(&record.id)
                        .ok_or("session_not_found")?
                        .reply(record.id),
                    None,
                ))
            }
            Operation::Close { session_id } => {
                self.session(owner, session_id)
                    .map_err(|_| "session_not_found")?;
                self.close(owner, session_id);
                Ok((
                    json!({"ok":true,"session_id":session_id,"state":"closed"}),
                    None,
                ))
            }
        }
    }
    fn profiles(&self) -> Vec<Value> {
        let profiles = if self.runtime.gpu_only() {
            vec![Profile::Sdr8, Profile::Hdr10]
        } else {
            vec![Profile::Sdr8]
        };
        profiles
            .into_iter()
            .map(|profile| {
                json!({
                    "name": "PyroWave",
                    "profile": profile,
                    "codec": format_json(profile.codec()),
                    "framing": format_json(RECORDS),
                    "options_schema": format_json(OPTIONS),
                    "bitstream": "186f0393",
                    "bit_depth": if profile.hdr() { 10 } else { 8 },
                    "chroma": "420",
                    "range": "full",
                    "matrix": if profile.hdr() { "bt2020_ncl" } else { "bt709" },
                    "inputs": if self.runtime.gpu_only() {
                        vec!["dmabuf"]
                    } else {
                        vec!["cpu_bgra8", "cpu_nv12"]
                    },
                    "mode": "submitted",
                    "provider_service": super::PROVIDER_SERVICE,
                    "consumer_service": super::CONSUMER_SERVICE,
                })
            })
            .collect()
    }
    fn open(
        &mut self,
        owner: &str,
        operation_id: String,
        configuration: Configuration,
    ) -> Result<(Value, Option<u64>), &'static str> {
        validate_operation(&operation_id)?;
        configuration.validate()?;
        if configuration.profile.hdr() && !self.runtime.gpu_only() {
            return Err("pyrowave_hdr_requires_gpu");
        }
        if let Some(record) = self
            .operations
            .iter()
            .find(|r| r.owner == owner && r.operation == operation_id)
        {
            if record.configuration != configuration {
                return Err("operation_conflict");
            }
            let session = self.sessions.get(&record.id).ok_or("session_closed")?;
            return Ok((session.reply(record.id), None));
        }
        if self.operations.len() >= MAX_OPERATIONS
            || self.sessions.len() >= MAX_SESSIONS
            || self.sessions.values().filter(|s| s.owner == owner).count() >= 2
        {
            return Err("session_limit");
        }
        let encoder = self
            .runtime
            .encoder(configuration.width, configuration.height)?;
        let id = self.next_id;
        self.next_id = id.checked_add(1).ok_or("session_limit")?;
        let session = Session {
            owner: owner.into(),
            configuration: configuration.clone(),
            encoder: Some(encoder),
            runtime: self.runtime.clone(),
            queue: OutputQueue::new(SessionMode::Submitted, 2, library::MAX_FRAME_BYTES)
                .map_err(|_| "encoder_state_failed")?,
            source: None,
            selection: None,
            last_sequence: 0,
            last_timestamp: None,
            draining: false,
        };
        let reply = session.reply(id);
        self.sessions.insert(id, session);
        self.operations.push(OperationRecord {
            owner: owner.into(),
            operation: operation_id,
            configuration,
            id,
        });
        Ok((reply, Some(id)))
    }
}
fn validate_operation(id: &str) -> Result<(), &'static str> {
    if id.is_empty() || id.len() > 128 || !id.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("invalid_operation_id");
    }
    Ok(())
}
fn format_json(id: FormatId) -> Value {
    json!({"namespace_id":id.namespace_id,"value":id.value,"schema":id.schema})
}
fn config(native: &encode::SessionConfig) -> Result<Configuration, Status> {
    if native.mode != SessionMode::Submitted
        || native.framing != RECORDS
        || native.options_schema != OPTIONS
        || native.source_id == 0
        || native.config_generation != 1
    {
        return Err(MEDIA_UNSUPPORTED);
    }
    let configuration: Configuration =
        serde_json::from_slice(&native.options).map_err(|_| STATUS_INVALID_ARGUMENT)?;
    configuration
        .validate()
        .map_err(|_| STATUS_INVALID_ARGUMENT)?;
    if native.codec != configuration.profile.codec() {
        return Err(MEDIA_UNSUPPORTED);
    }
    Ok(configuration)
}
fn probe(runtime: &Arc<Runtime>, configuration: &Configuration) -> Result<(), &'static str> {
    let _probe = runtime.begin_probe()?;
    if runtime.gpu_only() {
        #[cfg(feature = "pyrowave-gpu")]
        {
            runtime
                .gpu_context()
                .filter(|gpu| {
                    !gpu.offers(
                        configuration.width,
                        configuration.height,
                        configuration.profile.hdr(),
                    )
                    .is_empty()
                })
                .map(|_| ())
                .ok_or("pyrowave_gpu_format_unavailable")?;
            let mut encoder = runtime.encoder(configuration.width, configuration.height)?;
            return encoder.probe_gpu(configuration.profile.hdr(), configuration.budget());
        }
    }
    if configuration.profile.hdr() {
        return Err("pyrowave_hdr_requires_gpu");
    }
    let mut encoder = runtime.encoder(configuration.width, configuration.height)?;
    let pixels = vec![128; (configuration.width * configuration.height * 3 / 2) as usize];
    let buffer = nv12(
        &pixels,
        configuration.width,
        configuration.height,
        configuration.width,
    );
    // SAFETY: owned NV12 planes remain readable through synchronous encoding.
    unsafe { encoder.encode(&buffer, configuration.budget()) }?;
    Ok(())
}
pub(super) struct Provider {
    pub owner: String,
    pub shared: Shared,
}
impl EncoderProvider for Provider {
    fn probe(&self, native: encode::SessionConfig) -> Result<encode::ControlResponse, Status> {
        let configuration = config(&native)?;
        let runtime = self
            .shared
            .lock()
            .map_err(|_| MEDIA_BACKEND_ERROR)?
            .runtime
            .clone();
        probe(&runtime, &configuration).map_err(|error| {
            if error == "pyrowave_probe_busy" {
                MEDIA_BUSY
            } else {
                MEDIA_BACKEND_ERROR
            }
        })?;
        Ok(encode::ControlResponse {
            kind: 0,
            request_id: 0,
            effective_generation: 1,
            effective_submission_id: 0,
            outcome: ENCODER_OUTCOME_APPLIED,
            schema: OPTIONS,
            data: serde_json::to_vec(&configuration).map_err(|_| MEDIA_BACKEND_ERROR)?,
        })
    }
    fn configure(&self, id: u64, native: encode::SessionConfig) -> Result<(), Status> {
        let configuration = config(&native)?;
        let mut state = self.shared.lock().map_err(|_| MEDIA_BACKEND_ERROR)?;
        let session = state.session_mut(&self.owner, id)?;
        if session.configuration != configuration
            || session.draining
            || session.encoder.is_none()
            || session
                .source
                .is_some_and(|source| source != native.source_id)
        {
            return Err(MEDIA_UNSUPPORTED);
        }
        session.source = Some(native.source_id);
        Ok(())
    }
    fn output(&self, id: u64) -> Result<OutputQueue, Status> {
        Ok(self
            .shared
            .lock()
            .map_err(|_| MEDIA_BACKEND_ERROR)?
            .session(&self.owner, id)?
            .queue
            .clone())
    }
    fn notification_queue(&self, id: u64) -> Result<Option<OutputQueue>, Status> {
        let state = self.shared.lock().map_err(|_| MEDIA_BACKEND_ERROR)?;
        Ok(state.session(&self.owner, id).ok().map(|s| s.queue.clone()))
    }
    fn control(
        &self,
        id: u64,
        _: encode::ControlRequest,
    ) -> Result<encode::ControlResponse, Status> {
        self.shared
            .lock()
            .map_err(|_| MEDIA_BACKEND_ERROR)?
            .session(&self.owner, id)?;
        // Every frame is independently decodable; no optional control schema is offered.
        Err(MEDIA_UNSUPPORTED)
    }
    fn drain(&self, id: u64) -> Result<(), Status> {
        let mut state = self.shared.lock().map_err(|_| MEDIA_BACKEND_ERROR)?;
        let session = state.session_mut(&self.owner, id)?;
        if session.queue.snapshot().ended {
            return Ok(());
        }
        let source = session.source.ok_or(MEDIA_CLOSED)?;
        if session.encoder.is_none() {
            return Err(MEDIA_BACKEND_ERROR);
        }
        session.draining = true;
        session.queue.drain().map_err(|_| MEDIA_CLOSED)?;
        session
            .queue
            .finish(encode::EncodedPacket {
                flags: ENCODER_CHUNK_END_OF_STREAM,
                session_id: id,
                access_unit_id: 0,
                submission_id: 0,
                source_id: source,
                config_generation: 1,
                timestamp_us: session.last_timestamp.unwrap_or(0),
                codec: session.configuration.profile.codec(),
                framing: RECORDS,
                bytes: Vec::new(),
            })
            .map_err(|_| MEDIA_BUSY)
    }
    fn destroy(&self, id: u64) {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .close(&self.owner, id);
    }
    fn release(&self) -> Result<(), Status> {
        self.shared
            .lock()
            .map_err(|_| MEDIA_BACKEND_ERROR)?
            .close_owner(&self.owner);
        Ok(())
    }
}
impl Session {
    fn reply(&self, id: u64) -> Value {
        json!({"ok":true,"session_id":id,"configuration":self.configuration,
        "codec":format_json(self.configuration.profile.codec()),"framing":format_json(RECORDS),"options_schema":format_json(OPTIONS)})
    }
    pub fn dimensions(&self) -> (u32, u32) {
        (self.configuration.width, self.configuration.height)
    }
    pub fn configured(&self) -> bool {
        self.source.is_some() && !self.draining && self.encoder.is_some()
    }
    pub fn submit(
        &mut self,
        id: u64,
        frame: BorrowedResourceFrame<'_>,
        flags: u32,
    ) -> Result<(), Status> {
        if flags & !FRAME_RESOURCE_SUBMIT_FORCE_IDR != 0 {
            return Err(MEDIA_UNSUPPORTED);
        }
        if !self.configured() {
            return Err(MEDIA_CLOSED);
        }
        if self.queue.snapshot().output_ready {
            return Err(MEDIA_BUSY);
        }
        let descriptor = frame.descriptor();
        let timestamp = self.validate_frame(descriptor)?;
        let selection = self.selection.ok_or(MEDIA_UNSUPPORTED)?;
        let codec = self.configuration.profile.codec();
        let result = if let Some(caps) = selection.dmabuf {
            #[cfg(feature = "pyrowave-gpu")]
            {
                unsafe { self.encode_dmabuf(&frame, caps) }
            }
            #[cfg(not(feature = "pyrowave-gpu"))]
            {
                let _ = caps;
                Err("pyrowave_gpu_unavailable")
            }
        } else {
            if self.runtime.gpu_only() {
                return Err(MEDIA_UNSUPPORTED);
            }
            self.encode_cpu(&frame, selection.format)
        };
        let bytes = match result {
            Ok(bytes) => bytes,
            Err(_) => {
                self.encoder.take();
                return Err(MEDIA_BACKEND_ERROR);
            }
        };
        self.queue
            .push(encode::EncodedPacket {
                flags: ENCODER_CHUNK_ACCESS_UNIT_START
                    | ENCODER_CHUNK_ACCESS_UNIT_END
                    | ENCODER_CHUNK_KEYFRAME,
                session_id: id,
                access_unit_id: descriptor.sequence,
                submission_id: descriptor.sequence,
                source_id: self.source.expect("configured"),
                config_generation: 1,
                timestamp_us: timestamp,
                codec,
                framing: RECORDS,
                bytes,
            })
            .map_err(|_| MEDIA_BACKEND_ERROR)?;
        self.last_sequence = descriptor.sequence;
        self.last_timestamp = Some(timestamp);
        Ok(())
    }
    fn encode_cpu(
        &mut self,
        frame: &BorrowedResourceFrame<'_>,
        format: u32,
    ) -> Result<Vec<u8>, &'static str> {
        let configuration = &self.configuration;
        let owned = frame.copy_cpu().map_err(|_| "pyrowave_invalid_cpu_frame")?;
        let cpu = owned.pixels.borrow();
        if cpu.descriptor().format != format {
            return Err("pyrowave_invalid_cpu_format");
        }
        let converted;
        let (pixels, stride) = if format == PIXEL_BGRA8 {
            converted = super::input::bgra_to_nv12(
                cpu.pixels(),
                configuration.width,
                configuration.height,
                cpu.descriptor().stride,
            );
            (converted.as_slice(), configuration.width)
        } else {
            (cpu.pixels(), cpu.descriptor().stride)
        };
        let buffer = nv12(pixels, configuration.width, configuration.height, stride);
        // SAFETY: both the copied CPU planes and any conversion remain owned
        // until native upload finishes. No capture pointer reaches the GPU.
        unsafe {
            self.encoder
                .as_mut()
                .ok_or("pyrowave_session_closed")?
                .encode(&buffer, configuration.budget())
        }
    }
    #[cfg(feature = "pyrowave-gpu")]
    unsafe fn encode_dmabuf(
        &mut self,
        frame: &BorrowedResourceFrame<'_>,
        caps: DmaBufResourceCapabilities,
    ) -> Result<Vec<u8>, &'static str> {
        let resource = &frame.descriptor().resource;
        if resource.kind != FRAME_RESOURCE_DMABUF
            || resource.version != 1
            || resource.size as usize != size_of::<DmaBufFrame>()
            || resource.namespace != [0; 16]
            || resource.reserved != 0
            || resource.data.is_null()
            || !(resource.data as usize).is_multiple_of(std::mem::align_of::<DmaBufFrame>())
        {
            return Err("pyrowave_invalid_gpu_frame");
        }
        // SAFETY: the negotiated standard descriptor and its frame are borrowed
        // through this synchronous callback; no capture handle outlives it.
        let input = unsafe { &*resource.data.cast::<DmaBufFrame>() };
        let envelope = frame.descriptor();
        if input.validate().is_err()
            || input.fourcc != caps.fourcc
            || input.modifier != caps.modifier
            || input.color != caps.color
            || input.width != envelope.width
            || input.height != envelope.height
            || input.sequence != envelope.sequence
            || input.timestamp_us != envelope.timestamp_us
            || envelope.reserved != 0
            || !super::native::is_none(&envelope.acquire_sync)
        {
            return Err("pyrowave_invalid_gpu_frame");
        }
        // SAFETY: capture's immutable presentation remains lent through GPU
        // completion and import retirement, including failures.
        unsafe {
            self.encoder
                .as_mut()
                .ok_or("pyrowave_session_closed")?
                .encode_gpu(
                    input,
                    self.configuration.profile.hdr(),
                    self.configuration.budget(),
                )
        }
    }
    pub fn gpu_only(&self) -> bool {
        self.runtime.gpu_only()
    }
    #[cfg(feature = "pyrowave-gpu")]
    pub fn gpu_offers(&self) -> Vec<DmaBufResourceCapabilities> {
        self.runtime
            .gpu_context()
            .map(|gpu| {
                gpu.offers(
                    self.configuration.width,
                    self.configuration.height,
                    self.configuration.profile.hdr(),
                )
            })
            .unwrap_or_default()
    }
    fn validate_frame(&self, descriptor: &CaptureResourceFrame) -> Result<u64, Status> {
        let selection = self.selection.ok_or(MEDIA_UNSUPPORTED)?;
        let configuration = &self.configuration;
        let clock = &descriptor.clock;
        if descriptor.offer_generation != selection.generation
            || descriptor.offer_id != selection.id
            || (descriptor.width, descriptor.height) != self.dimensions()
            || descriptor.sequence <= self.last_sequence
            || clock.version != 1
            || clock.size as usize != size_of::<FrameClockMetadata>()
            || clock.domain != FRAME_CLOCK_HOST_MONOTONIC
            || clock.reserved != 0
            || clock.epoch_id != configuration.clock_epoch
            || descriptor.timestamp_us < configuration.clock_origin_us
        {
            return Err(MEDIA_UNSUPPORTED);
        }
        let timestamp = descriptor.timestamp_us - configuration.clock_origin_us;
        if self.last_timestamp.is_some_and(|last| timestamp <= last) {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        let color = descriptor.color;
        if color.version != 1
            || color.size as usize != size_of::<FrameColorMetadata>()
            || color.primaries
                != if self.configuration.profile.hdr() {
                    FRAME_PRIMARIES_BT2020
                } else {
                    FRAME_PRIMARIES_BT709
                }
            || color.range != FRAME_RANGE_FULL
            || (selection.dmabuf.is_some()
                && (color.matrix != FRAME_MATRIX_RGB
                    || color.transfer
                        != if self.configuration.profile.hdr() {
                            FRAME_TRANSFER_PQ
                        } else {
                            FRAME_TRANSFER_SRGB
                        }))
            || (selection.format == PIXEL_NV12
                && (color.transfer != FRAME_TRANSFER_SRGB || color.matrix != FRAME_MATRIX_BT709))
            || (selection.format == PIXEL_BGRA8
                && (color.transfer != FRAME_TRANSFER_SRGB || color.matrix != FRAME_MATRIX_RGB))
        {
            return Err(MEDIA_UNSUPPORTED);
        }
        Ok(timestamp)
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.queue.unsubscribe_native();
        self.queue.cancel();
        // Native destruction completes GPU work before the runtime can retire.
        self.encoder.take();
    }
}
fn nv12(bytes: &[u8], width: u32, height: u32, stride: u32) -> CpuBuffer {
    let y_size = (stride * height) as usize;
    CpuBuffer {
        data: [
            bytes.as_ptr().cast_mut().cast(),
            bytes[y_size..].as_ptr().cast_mut().cast(),
            std::ptr::null_mut(),
        ],
        stride: [stride as usize, stride as usize, 0],
        size: [y_size, bytes.len() - y_size, 0],
        width: width as i32,
        height: height as i32,
        format: 0,
    }
}
