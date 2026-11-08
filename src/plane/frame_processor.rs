//! The built-in video consumer uses the public SDK's canonical resource chain.
//! No capture backend, OS handle or processing allocation is interpreted here.
use super::*;
use amaterasu_module_sdk::{
    capture::ConnectionError,
    encode::{EncoderPresentation, EncoderResourceConsumer},
    frame_processing::{FrameProcessingSession, FrameProcessingStage},
};

pub(super) unsafe fn table(binding: &NativeBinding) -> Result<&FrameProcessorApi, &'static str> {
    // SAFETY: startup bound the exact FRAME_PROCESSOR contract identity.
    let api =
        unsafe { binding.table::<FrameProcessorApi>() }.map_err(|_| "frame_processor_interface")?;
    let header = api.base.base.base.header;
    if header.major != FRAME_PROCESSOR_INTERFACE_MAJOR
        || header.minor != FRAME_PROCESSOR_INTERFACE_MINOR
        || header.size < FRAME_PROCESSOR_API_SIZE
    {
        return Err("frame_processor_interface");
    }
    Ok(api)
}
pub(super) fn hdr_metadata(
    binding: &NativeBinding,
    stream: u64,
) -> Result<Option<HdrMetadata>, &'static str> {
    let api = unsafe { table(binding) }?;
    let mut metadata = HdrMetadata::default();
    match unsafe { (api.hdr_metadata)(api.base.base.base.context, stream, &mut metadata) } {
        STATUS_OK if metadata.enabled <= 1 && metadata.reserved == 0 => Ok(Some(metadata)),
        MEDIA_UNSUPPORTED => Ok(None),
        _ => Err("processor_hdr_metadata_unavailable"),
    }
}
pub(super) struct Session {
    // Cancellation precedes destruction of the stable encoder adapter table.
    connection: FrameProcessingSession,
    encoder: EncoderResourceConsumer,
    provider: CaptureProvider,
    capture: u64,
    encoder_service: String,
    encoder_session: u64,
}
impl Session {
    pub(super) unsafe fn new(
        capture: &CaptureApiV1_8,
        processor: &FrameProcessorApi,
        encoder: &EncoderApi,
        service: &str,
        input: &Pump<'_>,
    ) -> Result<Self, &'static str> {
        let adapter = unsafe { EncoderResourceConsumer::new(encoder) }
            .map_err(|_| "encoder_resource_interface_required")?;
        // NativePlane retains these exact bindings until this session cancels.
        // The adapter's boxed table stays at a stable address when moved here.
        let connection = unsafe {
            FrameProcessingSession::new(
                capture,
                input.capture,
                &[FrameProcessingStage {
                    processor: &processor.base,
                    stream: input.video,
                }],
                adapter.api(),
                input.encoder,
                0,
            )
        }
        .map_err(|_| "frame_processor_connection_invalid")?;
        Ok(Self {
            connection,
            encoder: adapter,
            provider: input.provider,
            capture: input.capture,
            encoder_service: service.to_owned(),
            encoder_session: input.encoder,
        })
    }
    pub(super) fn source_matches(&self, input: &Pump<'_>) -> bool {
        self.provider == input.provider && self.capture == input.capture
    }
    pub(super) unsafe fn select_encoder(
        &mut self,
        api: &EncoderApi,
        service: &str,
        session: u64,
    ) -> Result<(), &'static str> {
        if self.encoder_service == service && self.encoder_session == session {
            return Ok(());
        }
        let next = unsafe { EncoderResourceConsumer::new(api) }
            .map_err(|_| "encoder_resource_interface_required")?;
        unsafe { self.connection.replace_consumer(next.api(), session) }
            .map_err(|_| "frame_processor_consumer_retired")?;
        self.encoder = next;
        self.encoder_service = service.to_owned();
        self.encoder_session = session;
        Ok(())
    }
    pub(super) fn pump(&mut self, input: &Pump<'_>) -> Result<Progress, &'static str> {
        if !input.capture_due && !input.repeat_due {
            return Ok(Progress::default());
        }
        self.encoder.set_presentation(EncoderPresentation {
            timestamp_us: input.timestamp_us,
            sequence: input.sequence,
        });
        let flags = if input.force_idr {
            FRAME_RESOURCE_SUBMIT_FORCE_IDR
        } else {
            0
        };
        let started = Instant::now();
        let status = match self.connection.forward(flags) {
            Ok(status) => status,
            Err(ConnectionError::Native(MEDIA_BUSY | MEDIA_PENDING)) => {
                return Ok(Progress {
                    capture_deferred: true,
                    ..Progress::default()
                });
            }
            Err(ConnectionError::NoCompatibleOffer) => {
                return Err("frame_processor_resource_incompatible");
            }
            Err(ConnectionError::Stale) => return Err("frame_processor_resource_changed"),
            Err(_) => return Err("frame_processor_protocol"),
        };
        if !matches!(status, STATUS_OK | MEDIA_BUSY | MEDIA_PENDING) {
            return Err("frame_processor_failed");
        }
        let elapsed = elapsed_micros(started);
        let observed = self.connection.last_delivery();
        let fresh = observed.map(|frame| frame.freshness == CAPTURE_FRAME_FRESH);
        Ok(Progress {
            submitted: status == STATUS_OK,
            dropped: status == MEDIA_BUSY && observed.is_some(),
            capture_deferred: observed.is_none(),
            repeat_attempted: !input.capture_due,
            capture_reused: fresh == Some(false),
            dmabuf_fresh: fresh,
            dmabuf_presentation: observed.map(|frame| frame.sequence),
            frame: observed
                .filter(|_| status == STATUS_OK)
                .map(|frame| FrameMetadata {
                    width: frame.width,
                    height: frame.height,
                    stride: 0,
                    timestamp_us: input.timestamp_us,
                    sequence: input.sequence,
                }),
            capture_callback_us: elapsed,
            capture_callback_count: 1,
            capture_frame_count: u64::from(observed.is_some()),
            gpu_processor_us: observed.map(|_| self.connection.processor_dispatch_us()),
            encoder_submit_us: self.encoder.submit_us(),
            gpu_encoder_busy: status == MEDIA_BUSY && observed.is_some(),
            ..Progress::default()
        })
    }
}

#[cfg(test)]
mod tests;
