use crate::{
    factory::{Factory, LogForwardingFactory, NativeFactory},
    input::{GpuInput, InputSupport},
    layout,
    model::{AmdAv1LatencyMode, Configuration, EncoderIdentity, Packet},
    worker::Worker,
};
use amaterasu_abi::{STATUS_INVALID_ARGUMENT, STATUS_OK, Status};
use amaterasu_media::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use crate::worker::{Metrics as WorkerMetrics, ReceiveResult, SubmitResult};

pub trait Backend: Send {
    /// Begin idempotent cancellation without waiting for native work. The session remains
    /// owned until `ready_to_close` permits the mandatory close barrier.
    fn begin_close(&mut self) {}
    /// True only when closing will not wait for outstanding backend activity.
    /// Backends with asynchronous workers must override both close hooks.
    fn ready_to_close(&self) -> bool {
        true
    }
    /// Retire all native execution and source pixel reads before returning,
    /// including on error. An error preserves failed-cleanup evidence; it must
    /// never transfer detached work that could outlive this backend or module.
    fn close(self: Box<Self>) -> Result<(), &'static str> {
        drop(self);
        Ok(())
    }
    /// Tightly packed NV12 for eight-bit sessions or P010 for ten-bit sessions.
    /// Code values already match the negotiated output color; this is no conversion request.
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
    /// A side-effect-free diagnostic snapshot, if this backend supports it.
    fn observed_metrics(&self) -> Option<WorkerMetrics> {
        None
    }
    /// Starts a new frame report window after the current one was logged.
    fn reset_frame_window(&mut self) {}
    /// Inputs accepted besides packed NV12; read once when the session opens.
    fn input_support(&self) -> InputSupport {
        InputSupport::default()
    }
    /// True when the next `submit_gpu` cannot report busy.
    fn has_gpu_submit_capacity(&mut self) -> Result<bool, &'static str> {
        Ok(true)
    }
    /// Whether the backend can finish all source reads before GPU submit
    /// returns. Descriptor-only asynchronous backends must leave this false.
    fn supports_gpu_completion(&self) -> bool {
        false
    }
    fn submit_gpu(
        &mut self,
        _input: GpuInput,
        _timestamp: i64,
        _force: bool,
    ) -> Result<SubmitResult, &'static str> {
        Err("gpu_input_unsupported")
    }
}

impl Backend for Worker {
    fn begin_close(&mut self) {
        self.begin_shutdown();
    }
    fn ready_to_close(&self) -> bool {
        Worker::ready_to_close(self)
    }
    fn close(self: Box<Self>) -> Result<(), &'static str> {
        (*self).shutdown()
    }
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
    fn observed_metrics(&self) -> Option<WorkerMetrics> {
        Some(self.observed_metrics())
    }
    fn reset_frame_window(&mut self) {
        Worker::reset_frame_window(self);
    }
    fn input_support(&self) -> InputSupport {
        Worker::input_support(self)
    }
    fn has_gpu_submit_capacity(&mut self) -> Result<bool, &'static str> {
        Worker::has_gpu_submit_capacity(self)
    }
    fn supports_gpu_completion(&self) -> bool {
        Worker::supports_gpu_completion(self)
    }
    fn submit_gpu(
        &mut self,
        input: GpuInput,
        timestamp: i64,
        force: bool,
    ) -> Result<SubmitResult, &'static str> {
        Worker::submit_gpu(self, input, timestamp, force)
    }
}

/// Time finished packets spent waiting for the module to deliver them. Only the
/// host's tick loop stands between these two points, so a long wait here means
/// another module held that thread.
#[derive(Clone, Copy, Default, serde::Serialize)]
struct DeliveryWait {
    packets: u64,
    total_us: u64,
    max_us: u64,
}

impl DeliveryWait {
    fn record(&mut self, waited_us: u64) {
        self.packets = self.packets.saturating_add(1);
        self.total_us = self.total_us.saturating_add(waited_us);
        self.max_us = self.max_us.max(waited_us);
    }
}

struct Metadata {
    started_at: Instant,
    timing_recorded: bool,
    timestamp: i64,
    sequence: u64,
    force_keyframe: bool,
}

pub struct Session {
    packet_boundaries: bool,
    owner: String,
    config: Configuration,
    backend: Box<dyn Backend>,
    input_support: InputSupport,
    // Field order is deliberate: backend shutdown joins the worker before
    // retained provider references below are released on the module thread.
    metadata: VecDeque<Metadata>,
    retained_frames: BTreeMap<i64, RetainedFrame>,
    last_input: Option<(i64, u64)>,
    pending: Option<Packet>,
    draining: bool,
    closing: bool,
    eos_sent: bool,
    failed: bool,
    color_conversion_total_us: u64,
    color_conversion_max_us: u64,
    color_conversion_frames: u64,
    opened_at: Instant,
    encode_timing: EncodeTimingSnapshotV1,
    delivery_wait: DeliveryWait,
    /// Producer offer configured for resource frames (generation, offer).
    resource_producer: Option<(u64, u64)>,
}

/// Owned provider reference; callbacks stay on the module thread. Only duplicated
/// descriptors cross to the encoder worker, never this callback/context pair.
struct RetainedFrame {
    reference: RetainedReference,
    owner: std::thread::ThreadId,
}

/// A DMA-BUF lease or a negotiated resource frame (encoder v1.5).
enum RetainedReference {
    Lease(DmaBufFrameLease),
    Resource(CaptureResourceFrame),
}

/// Resource offers never change for a session: one generation, one offer.
const RESOURCE_GENERATION: u64 = 1;
const RESOURCE_OFFER: u64 = 1;
const NO_COMPLETION: FrameDescriptor = FrameDescriptor {
    kind: FRAME_SYNC_NONE,
    version: 0,
    size: 0,
    reserved: 0,
    namespace: [0; 16],
    data: std::ptr::null(),
};
// SAFETY: moving the record does not access its raw pointer. Drop checks thread
// affinity before invoking provider code; native entry points require calls on
// the owning module thread. Worker requests never contain this record.
unsafe impl Send for RetainedFrame {}
impl Drop for RetainedFrame {
    fn drop(&mut self) {
        // SAFETY: a successful retain owns this reference. Encoded output or
        // joined backend shutdown proves all GPU source reads have completed.
        assert_eq!(
            self.owner,
            std::thread::current().id(),
            "frame lease released off its module thread"
        );
        match &self.reference {
            RetainedReference::Lease(lease) => unsafe {
                (lease.release.expect("validated lease"))(lease.context, -1)
            },
            // SAFETY: as above; v1 CUDA completion is NONE after finished reads.
            RetainedReference::Resource(frame) => unsafe {
                (frame.release)(frame.context, &NO_COMPLETION)
            },
        }
    }
}

struct OpenRecord {
    packet_boundaries: bool,
    id: u64,
    encoder: EncoderIdentity,
    configuration: Configuration,
}

struct CloseFailure {
    owner: String,
    code: &'static str,
}

pub struct Sessions {
    pub enabled: bool,
    factory: Arc<dyn Factory>,
    next_id: u64,
    operations: BTreeMap<(String, String), OpenRecord>,
    values: BTreeMap<u64, Session>,
    close_failures: BTreeMap<u64, CloseFailure>,
    closed: BTreeMap<u64, String>,
    accepting_new_sessions: bool,
}

impl Sessions {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            factory: Arc::new(NativeFactory),
            next_id: 0,
            operations: BTreeMap::new(),
            values: BTreeMap::new(),
            close_failures: BTreeMap::new(),
            closed: BTreeMap::new(),
            accepting_new_sessions: true,
        }
    }
    pub fn with_log_emitter(
        enabled: bool,
        emitter: amaterasu_module_sdk::LogEmitter,
        notifier: amaterasu_module_sdk::WorkNotifier,
    ) -> Self {
        let mut sessions = Self::new(enabled);
        sessions.factory = Arc::new(LogForwardingFactory::new(emitter, notifier));
        sessions
    }
    #[cfg(test)]
    pub fn with_factory(enabled: bool, factory: Arc<dyn Factory>) -> Self {
        Self {
            factory,
            ..Self::new(enabled)
        }
    }
    pub fn enumerate(&self) -> Result<Vec<crate::model::EncoderDescription>, &'static str> {
        if self.enabled {
            self.factory.enumerate()
        } else {
            Ok(Vec::new())
        }
    }
    pub fn probe_factory(
        &self,
        owner: &str,
        encoder: &EncoderIdentity,
        configuration: &Configuration,
    ) -> Result<Arc<dyn Factory>, &'static str> {
        self.validate_open(owner, encoder, configuration)?;
        Ok(self.factory.clone())
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
                if let Some(failure) = self.close_failures.get(&record.id) {
                    return Err(failure.code);
                }
                match self.values.get(&record.id) {
                    Some(session) if session.closing => return Err("session_closing"),
                    Some(_) => (),
                    None => return Err("session_closed"),
                }
                return Ok(open_reply(record.id, &configuration, packet_boundaries));
            }
            if self.operations.len() >= 128 {
                return Err("operation_limit");
            }
        }
        self.validate_open(owner, &encoder, &configuration)?;
        let backend = self.factory.open(encoder.clone(), configuration.clone())?;
        let response = self.insert(owner, configuration.clone(), backend, packet_boundaries)?;
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
        if !self.accepting_new_sessions {
            return Err("encoder_shutting_down");
        }
        config.validate()?;
        encoder.validate()?;
        if self.values.len() + self.close_failures.len() + self.closed.len() >= 8
            || self
                .values
                .values()
                .filter(|session| session.owner == owner)
                .count()
                + self
                    .close_failures
                    .values()
                    .filter(|failure| failure.owner == owner)
                    .count()
                + self
                    .closed
                    .values()
                    .filter(|closed_owner| *closed_owner == owner)
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
        let packet_boundaries = packet_boundaries || configuration.codec() != VideoCodec::H264;
        let response = open_reply(self.next_id, &configuration, packet_boundaries);
        let input_support = backend.input_support();
        self.values.insert(
            self.next_id,
            Session {
                packet_boundaries,
                owner: owner.into(),
                config: configuration,
                backend,
                input_support,
                metadata: VecDeque::new(),
                retained_frames: BTreeMap::new(),
                last_input: None,
                pending: None,
                draining: false,
                closing: false,
                eos_sent: false,
                failed: false,
                color_conversion_total_us: 0,
                color_conversion_max_us: 0,
                color_conversion_frames: 0,
                opened_at: Instant::now(),
                encode_timing: EncodeTimingSnapshotV1::empty(1),
                delivery_wait: DeliveryWait::default(),
                resource_producer: None,
            },
        );
        Ok(response)
    }
    fn owned(&mut self, owner: &str, id: u64) -> Result<&mut Session, &'static str> {
        if let Some(failure) = self.close_failures.get(&id)
            && failure.owner == owner
        {
            return Err(failure.code);
        }
        self.values
            .get_mut(&id)
            .filter(|session| session.owner == owner)
            .ok_or("session_not_found")
    }
    pub fn encode_timing(
        &mut self,
        owner: &str,
        id: u64,
    ) -> Result<EncodeTimingSnapshotV1, Status> {
        self.owned(owner, id)
            .map(|session| session.encode_timing)
            .map_err(|_| MEDIA_CLOSED)
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
        let state = if session.closing {
            "closing"
        } else if session.failed {
            "failed"
        } else if session.draining {
            "draining"
        } else {
            "open"
        };
        Ok(
            json!({"ok":true,"session_id":id,"state":state,"configuration":session.config,"packet_boundaries":session.packet_boundaries,"supports_packet_boundaries":true,"timing":{"color_conversion_frames":session.color_conversion_frames,"color_conversion_total_us":session.color_conversion_total_us,"color_conversion_max_us":session.color_conversion_max_us},"encode_latency":session.encode_timing,"encoder_timing":encoder_metrics}),
        )
    }

    /// A bounded, host-only snapshot; worker timings are cumulative since the
    /// session opened and do not measure capture, packet delivery or display.
    pub fn timing_summaries(&mut self, now: Instant) -> Vec<String> {
        self.values
            .iter_mut()
            .filter_map(|(id, session)| {
                let metrics = session.backend.observed_metrics()?;
                let elapsed_ms = now.saturating_duration_since(session.opened_at).as_millis();
                let delivery_wait = std::mem::take(&mut session.delivery_wait);
                session.backend.reset_frame_window();
                Some(format!(
                    "encoder_session={id} elapsed_ms={elapsed_ms} width={} height={} target_fps={} delivery_wait={} worker_metrics_cumulative={}",
                    session.config.width,
                    session.config.height,
                    session.config.fps,
                    serde_json::to_string(&delivery_wait).ok()?,
                    serde_json::to_string(&metrics).ok()?
                ))
            })
            .collect()
    }
    /// Synchronous lifetime barrier for native cancellation and final teardown.
    /// Broker Close uses begin_close/poll_closing and keeps its request pending
    /// until the worker is ready, rather than waiting inside the host tick.
    pub fn close(&mut self, owner: &str, id: u64) -> Result<Value, &'static str> {
        if self
            .closed
            .get(&id)
            .is_some_and(|closed_owner| closed_owner == owner)
        {
            return Ok(close_reply(id));
        }
        self.owned(owner, id)?;
        let session = self.values.remove(&id).expect("validated owned session");
        if let Err(code) = session.close() {
            self.close_failures.insert(
                id,
                CloseFailure {
                    owner: owner.to_owned(),
                    code,
                },
            );
            return Err(code);
        }
        self.closed.insert(id, owner.into());
        Ok(close_reply(id))
    }

    pub fn begin_close(&mut self, owner: &str, id: u64) -> Result<(), &'static str> {
        if self
            .closed
            .get(&id)
            .is_some_and(|closed_owner| closed_owner == owner)
        {
            return Ok(());
        }
        self.owned(owner, id)?.begin_close();
        Ok(())
    }

    pub fn close_result(&self, owner: &str, id: u64) -> Option<Result<Value, &'static str>> {
        if self
            .closed
            .get(&id)
            .is_some_and(|closed_owner| closed_owner == owner)
        {
            return Some(Ok(close_reply(id)));
        }
        if let Some(failure) = self.close_failures.get(&id)
            && failure.owner == owner
        {
            return Some(Err(failure.code));
        }
        if self
            .values
            .get(&id)
            .is_some_and(|session| session.owner == owner)
        {
            None
        } else {
            Some(Err("session_not_found"))
        }
    }

    pub fn release_close_receipts(&mut self) {
        self.closed.clear();
    }
    pub fn lookup(&self, owner: &str, operation: &str) -> Result<Value, &'static str> {
        validate_operation(operation)?;
        let record = self
            .operations
            .get(&(owner.into(), operation.into()))
            .ok_or("session_not_found")?;
        if let Some(failure) = self.close_failures.get(&record.id) {
            return Err(failure.code);
        }
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
    pub fn stop_admitting_sessions(&mut self) {
        self.accepting_new_sessions = false;
    }

    pub fn begin_shutdown(&mut self) {
        self.stop_admitting_sessions();
        for session in self.values.values_mut() {
            session.begin_close();
        }
    }

    pub fn poll_closing(&mut self) {
        let ready: Vec<_> = self
            .values
            .iter()
            .filter(|(_, session)| session.closing && session.backend.ready_to_close())
            .map(|(id, session)| (*id, session.owner.clone()))
            .collect();
        for (id, owner) in ready {
            let _ = self.close(&owner, id);
        }
    }

    pub fn shutdown_status(&self) -> Result<bool, &'static str> {
        if !self.values.is_empty() {
            return Ok(false);
        }
        match self.close_failures.values().next() {
            Some(failure) => Err(failure.code),
            None => Ok(true),
        }
    }

    pub fn clear(&mut self) -> Result<(), &'static str> {
        self.stop_admitting_sessions();
        for session in self.values.values_mut() {
            session.begin_close();
        }
        let sessions: Vec<_> = self
            .values
            .iter()
            .map(|(id, session)| (*id, session.owner.clone()))
            .collect();
        for (id, owner) in sessions {
            let _ = self.close(&owner, id);
        }
        match self.close_failures.values().next() {
            Some(failure) => Err(failure.code),
            None => Ok(()),
        }
    }
    /// # Safety
    /// Frame allocation is borrowed for this call and contains its declared bytes.
    pub unsafe fn submit(&mut self, owner: &str, id: u64, frame: &CpuFrame, force: u32) -> Status {
        let Ok(session) = self.owned(owner, id) else {
            return MEDIA_CLOSED;
        };
        let timestamp = match session.admit(
            frame.width,
            frame.height,
            frame.timestamp_us,
            frame.sequence,
            force,
        ) {
            Ok(timestamp) => timestamp,
            Err(status) => return status,
        };
        // SAFETY: native caller guarantees the allocation; validation precedes read.
        let bytes = match unsafe { frame.bytes() } {
            Ok(bytes) => bytes,
            Err(status) => return status,
        };
        if !layout::input_color_supported(frame.format, session.config.color_mode) {
            return STATUS_INVALID_ARGUMENT;
        }
        let started_at = Instant::now();
        let result = if frame.format == PIXEL_BGRA8 && session.input_support.gpu_bgra_conversion {
            let Ok(packed) = layout::packed_bgra(
                bytes,
                frame.width as usize,
                frame.height as usize,
                frame.stride as usize,
            ) else {
                return STATUS_INVALID_ARGUMENT;
            };
            session
                .backend
                .submit_gpu(GpuInput::Bgra(packed), timestamp, force != 0)
        } else {
            let planar = match session.packed_cpu_input(frame, bytes) {
                Ok(planar) => planar,
                Err(status) => return status,
            };
            session.backend.submit(planar, timestamp, force != 0)
        };
        session.record_submission(result, timestamp, frame.sequence, force != 0, started_at)
    }
    /// # Safety
    /// Every descriptor in `frame` is open for the duration of this call.
    pub unsafe fn submit_dmabuf(
        &mut self,
        owner: &str,
        id: u64,
        frame: &DmaBufFrame,
        force: u32,
    ) -> Status {
        let Ok(session) = self.owned(owner, id) else {
            return MEDIA_CLOSED;
        };
        if let Err(status) = session.check_accepting() {
            return status;
        }
        #[cfg(unix)]
        {
            // SAFETY: forwarded caller guarantee for the borrowed descriptors.
            unsafe { session.submit_dmabuf(frame, force, None) }
        }
        #[cfg(not(unix))]
        {
            let _ = (frame, force);
            MEDIA_UNSUPPORTED
        }
    }
    /// Retains immutable pixels until encoded output or joined worker shutdown.
    ///
    /// # Safety
    /// The provider's initial lease, callbacks and immutable storage must remain
    /// live until the retained reference is released on the module thread.
    pub unsafe fn submit_dmabuf_leased(
        &mut self,
        owner: &str,
        id: u64,
        lease: &DmaBufFrameLease,
        force: u32,
    ) -> Status {
        if let Err(status) = lease.validate() {
            return status;
        }
        let Ok(session) = self.owned(owner, id) else {
            return MEDIA_CLOSED;
        };
        if let Err(status) = session.check_accepting() {
            return status;
        }
        #[cfg(unix)]
        {
            // SAFETY: caller holds the lease's immutable storage until return.
            unsafe { session.submit_dmabuf(&lease.frame, force, Some(lease)) }
        }
        #[cfg(not(unix))]
        {
            let _ = (session, force);
            MEDIA_UNSUPPORTED
        }
    }

    /// Advisory only: a concurrent submission may consume the observed space.
    /// Encoder v1.5: offers CUDA vendor-schema input when this session can
    /// convert it. An empty list (with its generation) means the lease path.
    ///
    /// # Safety
    /// The sink and generation output must be live for this call.
    pub unsafe fn enumerate_resources(
        &mut self,
        owner: &str,
        id: u64,
        sink: CaptureResourceOfferSink,
        generation: &mut u64,
    ) -> Status {
        let Ok(session) = self.owned(owner, id) else {
            return MEDIA_CLOSED;
        };
        if let Err(status) = session.check_accepting() {
            return status;
        }
        *generation = RESOURCE_GENERATION;
        let Some(payloads) = session.cuda_offer_payloads() else {
            return STATUS_OK;
        };
        let offer = cuda_offer(&payloads);
        // SAFETY: the caller's sink is live; payloads outlive the callback.
        unsafe { (sink.write)(sink.context, &offer) }
    }

    /// Accepts a producer offer this session's own offer can consume.
    ///
    /// # Safety
    /// The producer offer and its payloads must be live for this call.
    pub unsafe fn configure_resource(
        &mut self,
        owner: &str,
        id: u64,
        consumer_generation: u64,
        consumer_offer: u64,
        producer_generation: u64,
        producer: &CaptureResourceOffer,
    ) -> Status {
        let Ok(session) = self.owned(owner, id) else {
            return MEDIA_CLOSED;
        };
        if let Err(status) = session.check_accepting() {
            return status;
        }
        if consumer_generation != RESOURCE_GENERATION
            || consumer_offer != RESOURCE_OFFER
            || producer_generation == 0
            || producer.id == 0
        {
            return MEDIA_UNSUPPORTED;
        }
        let Some(payloads) = session.cuda_offer_payloads() else {
            return MEDIA_UNSUPPORTED;
        };
        // SAFETY: the producer offer is borrowed live; ours lives on this frame.
        if !unsafe { cuda_offers_compatible(producer, &cuda_offer(&payloads)) } {
            return MEDIA_UNSUPPORTED;
        }
        session.resource_producer = Some((producer_generation, producer.id));
        STATUS_OK
    }

    /// Encodes a frame of the configured producer offer. OK retains it until
    /// this picture's encoded output; other results retain nothing.
    ///
    /// # Safety
    /// The frame, its payloads and callbacks must be live for this call.
    pub unsafe fn submit_resource(
        &mut self,
        owner: &str,
        id: u64,
        frame: &CaptureResourceFrame,
        flags: u32,
    ) -> Status {
        let Ok(session) = self.owned(owner, id) else {
            return MEDIA_CLOSED;
        };
        if let Err(status) = session.check_accepting() {
            return status;
        }
        if flags & !FRAME_RESOURCE_SUBMIT_FORCE_IDR != 0 {
            return MEDIA_UNSUPPORTED;
        }
        if session.resource_producer != Some((frame.offer_generation, frame.offer_id)) {
            return MEDIA_UNSUPPORTED;
        }
        // SAFETY: the caller keeps the frame and payloads live for this call.
        unsafe { session.submit_cuda(frame, flags & FRAME_RESOURCE_SUBMIT_FORCE_IDR) }
    }

    pub fn gpu_input_ready(&mut self, owner: &str, id: u64) -> Status {
        let Ok(session) = self.owned(owner, id) else {
            return MEDIA_CLOSED;
        };
        if let Err(status) = session.check_accepting() {
            return status;
        }
        if session.input_support.dmabuf.is_none() || !session.backend.supports_gpu_completion() {
            return MEDIA_UNSUPPORTED;
        }
        if session.metadata.len() >= 16 {
            return MEDIA_BUSY;
        }
        match session.backend.has_gpu_submit_capacity() {
            Ok(true) => STATUS_OK,
            Ok(false) => MEDIA_BUSY,
            Err(_) => {
                session.failed = true;
                MEDIA_BACKEND_ERROR
            }
        }
    }

    pub fn drain(&mut self, owner: &str, id: u64) -> Status {
        let Ok(session) = self.owned(owner, id) else {
            return MEDIA_CLOSED;
        };
        if session.closing {
            return MEDIA_CLOSED;
        }
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
        if session.closing {
            return MEDIA_CLOSED;
        }
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
                codec: session.config.codec().abi_id(),
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
            .iter_mut()
            .find(|metadata| metadata.timestamp == packet.timestamp_100ns)
        else {
            session.failed = true;
            return MEDIA_BACKEND_ERROR;
        };
        if metadata.force_keyframe && !packet.keyframe {
            session.failed = true;
            return MEDIA_BACKEND_ERROR;
        }
        if !metadata.timing_recorded {
            if let Some(elapsed) = packet
                .ready_at
                .and_then(|ready| ready.checked_duration_since(metadata.started_at))
                && let Ok(micros) = u64::try_from(elapsed.as_micros())
            {
                session.encode_timing.record(micros);
            }
            if let Some(waited) = packet
                .ready_at
                .map(|ready| ready.elapsed().as_micros().min(u128::from(u64::MAX)) as u64)
            {
                session.delivery_wait.record(waited);
            }
            metadata.timing_recorded = true;
        }
        session.retained_frames.remove(&packet.timestamp_100ns);
        let encoded = EncodedPacket {
            codec: session.config.codec().abi_id(),
            // Encoder packets are whole access units (AV1: temporal units).
            // HEVC and AV1 always mark the final fragment, even without the
            // H.264 boundary opt-in.
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

fn close_reply(id: u64) -> Value {
    json!({"ok":true,"session_id":id,"state":"closed"})
}

impl Session {
    fn begin_close(&mut self) {
        if !self.closing {
            self.closing = true;
            self.backend.begin_close();
        }
    }

    fn close(self) -> Result<(), &'static str> {
        let Self {
            backend,
            retained_frames,
            ..
        } = self;
        // Backend::close is the source-read barrier on both success and error.
        // Provider callbacks must remain pinned until that barrier completes.
        let result = backend.close();
        drop(retained_frames);
        result
    }

    fn check_accepting(&self) -> Result<(), Status> {
        if self.closing {
            return Err(MEDIA_CLOSED);
        }
        if self.failed {
            return Err(MEDIA_BACKEND_ERROR);
        }
        if self.draining {
            return Err(MEDIA_CLOSED);
        }
        Ok(())
    }

    /// Admission shared by every input kind; returns the 100 ns timestamp.
    /// A rejected frame retains neither its timestamp nor its sequence.
    fn admit(
        &self,
        width: u32,
        height: u32,
        timestamp_us: u64,
        sequence: u64,
        force: u32,
    ) -> Result<i64, Status> {
        self.check_accepting()?;
        if self.metadata.len() >= 16 {
            return Err(MEDIA_BUSY);
        }
        if width != self.config.width || height != self.config.height || force > 1 {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        let timestamp = timestamp_us
            .checked_mul(10)
            .and_then(|time| i64::try_from(time).ok())
            .ok_or(STATUS_INVALID_ARGUMENT)?;
        if self
            .last_input
            .is_some_and(|(last_timestamp, last_sequence)| {
                timestamp <= last_timestamp || sequence <= last_sequence
            })
        {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        Ok(timestamp)
    }

    /// Preserve explicitly selected planar bit depth. Backends already receive
    /// NV12 for eight-bit configurations and packed P010 for ten-bit probes.
    fn packed_cpu_input(&mut self, frame: &CpuFrame, bytes: &[u8]) -> Result<Vec<u8>, Status> {
        if self.config.is_ten_bit() {
            return if frame.format == PIXEL_P010 {
                layout::p010(
                    bytes,
                    frame.width as usize,
                    frame.height as usize,
                    frame.stride as usize,
                )
                .map_err(|_| STATUS_INVALID_ARGUMENT)
            } else {
                Err(MEDIA_UNSUPPORTED)
            };
        }
        if frame.format == PIXEL_P010 {
            return Err(MEDIA_UNSUPPORTED);
        }
        let conversion_started = Instant::now();
        let nv12 = match frame.format {
            PIXEL_BGRA8 => layout::bgra(
                bytes,
                frame.width as usize,
                frame.height as usize,
                frame.stride as usize,
                self.config.color_mode,
            ),
            PIXEL_NV12 => layout::nv12(
                bytes,
                frame.width as usize,
                frame.height as usize,
                frame.stride as usize,
            ),
            _ => return Err(MEDIA_UNSUPPORTED),
        }
        .map_err(|_| STATUS_INVALID_ARGUMENT)?;
        if frame.format == PIXEL_BGRA8 {
            let conversion_us = conversion_started
                .elapsed()
                .as_micros()
                .min(u128::from(u64::MAX)) as u64;
            self.color_conversion_frames = self.color_conversion_frames.saturating_add(1);
            self.color_conversion_total_us =
                self.color_conversion_total_us.saturating_add(conversion_us);
            self.color_conversion_max_us = self.color_conversion_max_us.max(conversion_us);
        }
        Ok(nv12)
    }

    /// Validates, then duplicates descriptors only once acceptance is
    /// certain: MEDIA_BUSY and MEDIA_UNSUPPORTED duplicate nothing.
    ///
    /// # Safety
    /// Every descriptor in `frame` is open for the duration of this call.
    /// The CUDA input offer's payloads, when this session can convert it.
    fn cuda_offer_payloads(&self) -> Option<(CudaDeviceCapabilities, CudaEventCapabilities)> {
        let device_uuid = self.input_support.cuda_device?;
        self.backend.supports_gpu_completion().then_some((
            CudaDeviceCapabilities {
                device_uuid,
                format: CUDA_FORMAT_X2B10G10R10,
                min_width: self.config.width,
                max_width: self.config.width,
                min_height: self.config.height,
                max_height: self.config.height,
                reserved: 0,
            },
            CudaEventCapabilities { device_uuid },
        ))
    }

    /// # Safety
    /// The frame, its payloads and callbacks must be live for this call.
    unsafe fn submit_cuda(&mut self, frame: &CaptureResourceFrame, force: u32) -> Status {
        let timestamp = match self.admit(
            frame.width,
            frame.height,
            frame.timestamp_us,
            frame.sequence,
            force,
        ) {
            Ok(timestamp) => timestamp,
            Err(status) => return status,
        };
        // SAFETY: the caller keeps the descriptors' payloads live for this call.
        let (Some(pixels), Some(ready)) = (unsafe { cuda_device_frame(&frame.resource) }, unsafe {
            cuda_event_sync(&frame.acquire_sync)
        }) else {
            return STATUS_INVALID_ARGUMENT;
        };
        if Some(pixels.device_uuid) != self.input_support.cuda_device
            || pixels.format != CUDA_FORMAT_X2B10G10R10
            || pixels.width != frame.width
            || pixels.height != frame.height
            || pixels.device_pointer == 0
            || ready.event.is_null()
        {
            return STATUS_INVALID_ARGUMENT;
        }
        match self.backend.has_gpu_submit_capacity() {
            Ok(true) => {}
            Ok(false) => return MEDIA_BUSY,
            Err(_) => {
                self.failed = true;
                return MEDIA_BACKEND_ERROR;
            }
        }
        // SAFETY: the producer keeps its callbacks live; a failed retain
        // transfers nothing.
        let status = unsafe { (frame.retain)(frame.context) };
        if status != STATUS_OK {
            return status;
        }
        let retained = RetainedFrame {
            reference: RetainedReference::Resource(*frame),
            owner: std::thread::current().id(),
        };
        let input = crate::input::CudaInput {
            pointer: pixels.device_pointer,
            pitch: pixels.pitch,
            width: pixels.width,
            height: pixels.height,
            ready: ready.event as usize,
        };
        let started_at = Instant::now();
        let result = self
            .backend
            .submit_gpu(GpuInput::Cuda(input), timestamp, force != 0);
        let status =
            self.record_submission(result, timestamp, frame.sequence, force != 0, started_at);
        if status == STATUS_OK {
            self.retained_frames.insert(timestamp, retained);
        }
        status
    }

    #[cfg(unix)]
    unsafe fn submit_dmabuf(
        &mut self,
        frame: &DmaBufFrame,
        force: u32,
        lease: Option<&DmaBufFrameLease>,
    ) -> Status {
        let Some(import) = self.input_support.dmabuf.clone() else {
            return MEDIA_UNSUPPORTED;
        };
        let timestamp = match self.admit(
            frame.width,
            frame.height,
            frame.timestamp_us,
            frame.sequence,
            force,
        ) {
            Ok(timestamp) => timestamp,
            Err(status) => return status,
        };
        if let Err(status) = frame.validate() {
            return status;
        }
        let expected_color = if self.config.is_hdr() {
            DMABUF_COLOR_BT2020_PQ
        } else {
            DMABUF_COLOR_SRGB
        };
        if frame.color != expected_color || !import.supports(frame.fourcc, frame.modifier) {
            return MEDIA_UNSUPPORTED;
        }
        match self.backend.has_gpu_submit_capacity() {
            Ok(true) => {}
            Ok(false) => return MEDIA_BUSY,
            Err(_) => {
                self.failed = true;
                return MEDIA_BACKEND_ERROR;
            }
        }
        if lease.is_some() && !self.backend.supports_gpu_completion() {
            return MEDIA_UNSUPPORTED;
        }
        // SAFETY: validated shape; the caller keeps the descriptors open.
        let mut input = match unsafe { crate::input::DmaBufInput::duplicate(frame) } {
            Ok(input) => input,
            Err(status) => return status,
        };
        // A retained lease is released only with this frame's encoded output
        // or after the backend joins shutdown, both after its GPU reads.
        input.source_retained = lease.is_some();
        let retained = if let Some(lease) = lease {
            // SAFETY: validated provider callbacks and initial reference remain
            // live during this call. Failed retain transfers no ownership.
            let status = unsafe { (lease.retain.expect("validated lease"))(lease.context) };
            if status != STATUS_OK {
                return status;
            }
            Some(RetainedFrame {
                reference: RetainedReference::Lease(*lease),
                owner: std::thread::current().id(),
            })
        } else {
            None
        };
        let started_at = Instant::now();
        let result = self
            .backend
            .submit_gpu(GpuInput::DmaBuf(input), timestamp, force != 0);
        let status =
            self.record_submission(result, timestamp, frame.sequence, force != 0, started_at);
        if status == STATUS_OK
            && let Some(retained) = retained
        {
            self.retained_frames.insert(timestamp, retained);
        }
        status
    }

    fn record_submission(
        &mut self,
        result: Result<SubmitResult, &'static str>,
        timestamp: i64,
        sequence: u64,
        force_keyframe: bool,
        started_at: Instant,
    ) -> Status {
        match result {
            Ok(SubmitResult::Accepted) => {}
            Ok(SubmitResult::Busy) => return MEDIA_BUSY,
            Err(_) => {
                self.failed = true;
                return MEDIA_BACKEND_ERROR;
            }
        }
        self.metadata.push_back(Metadata {
            started_at,
            timing_recorded: false,
            timestamp,
            sequence,
            force_keyframe,
        });
        self.last_input = Some((timestamp, sequence));
        STATUS_OK
    }
}

/// This session's CUDA input offer; descriptors point into `payloads`.
fn cuda_offer(payloads: &(CudaDeviceCapabilities, CudaEventCapabilities)) -> CaptureResourceOffer {
    CaptureResourceOffer {
        id: RESOURCE_OFFER,
        ownership: FRAME_OWNERSHIP_RETAINABLE,
        accessibility: FRAME_ACCESS_GPU,
        reserved: 0,
        resource: cuda_descriptor(FRAME_RESOURCE_CUDA_DEVICE, &payloads.0),
        acquire_sync: cuda_descriptor(FRAME_SYNC_CUDA_EVENT, &payloads.1),
        completion_sync: NO_COMPLETION,
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
    let packet_boundaries = packet_boundaries || config.codec() != VideoCodec::H264;
    json!({"ok":true,"session_id":id,"configuration":config,"codec":match config.codec() { VideoCodec::H264 => "h264", VideoCodec::Hevc => "hevc", VideoCodec::Av1 => "av1" },"max_reference_frames":config.max_reference_frames,"packet_boundaries":packet_boundaries,"supports_packet_boundaries":true})
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod shutdown_tests;

#[cfg(all(test, unix))]
mod gpu_input_tests;
