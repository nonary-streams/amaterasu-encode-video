//! Selected standard-CPU processing through the same public SDK external
//! consumers use. Source provenance and output color remain independent.
use super::*;
use crate::model::GpuProcessing;
use amaterasu_module_sdk::{
    DynamicRange,
    capture::{CpuSourceFormat, SourceFormat},
    processing::resource::{CpuProcessingTarget, CpuResourceProcessingSession},
};

pub(super) struct Processor {
    // Sessions cancel before their retained processor binding is released.
    sessions: BTreeMap<u64, CpuResourceProcessingSession>,
    active: BTreeSet<u64>,
    binding: NativeBinding,
    service: String,
}
impl Processor {
    pub(super) fn bind(runtime: &RuntimeClient, service: &str) -> ModuleResult<Self> {
        // SAFETY: signed startup composition selects this service and exact
        // family, and the retained binding outlives every synchronous session.
        let binding = unsafe {
            runtime.bind_native_typed(
                service,
                native_contracts::FRAME_RESOURCE_PROCESSOR,
                FRAME_RESOURCE_PROCESSOR_INTERFACE_MAJOR,
                FRAME_RESOURCE_PROCESSOR_INTERFACE_MINOR,
                FRAME_RESOURCE_PROCESSOR_API_V1_0_SIZE,
            )
        }?;
        let api = unsafe { binding.table::<FrameResourceProcessorApi>() }?;
        if api.base.base.header.major != FRAME_RESOURCE_PROCESSOR_INTERFACE_MAJOR {
            return Err("resource processor interface mismatch".into());
        }
        Ok(Self {
            sessions: BTreeMap::new(),
            active: BTreeSet::new(),
            binding,
            service: service.into(),
        })
    }
    pub(super) fn configure(
        &mut self,
        stream: u64,
        selection: Option<&GpuProcessing>,
    ) -> Result<(), &'static str> {
        let Some(selection) = selection else {
            self.finish(stream);
            return Ok(());
        };
        if selection.service != self.service {
            return Err("resource_processor_selection_mismatch");
        }
        if !selection.settings.is_empty() || selection.live_settings.is_some() {
            return Err("resource_processor_stream_settings_unsupported");
        }
        if selection.enabled {
            if self.active.len()
                >= amaterasu_module_sdk::processing::resource::MAX_PROCESSING_STREAMS
                && !self.active.contains(&stream)
            {
                return Err("resource_processor_capacity");
            }
            self.active.insert(stream);
        } else {
            self.finish(stream);
        }
        Ok(())
    }
    pub(super) fn is_active(&self, stream: u64) -> bool {
        self.active.contains(&stream)
    }
    pub(super) fn finish(&mut self, stream: u64) {
        self.sessions.remove(&stream);
        self.active.remove(&stream);
    }
    pub(super) fn finish_all(&mut self) {
        self.sessions.clear();
        self.active.clear();
    }
    pub(super) fn hdr_metadata(&self, stream: u64) -> Option<HdrMetadata> {
        let output = self.sessions.get(&stream)?.output_format()?;
        // v1.0 has no static HDR metadata query. Known SDR output explicitly
        // disables HDR; a transformed HDR output keeps mastering data unknown.
        (output.color.transfer != FRAME_TRANSFER_PQ).then_some(HdrMetadata::default())
    }
    pub(super) fn pump(
        &mut self,
        capture: &CaptureApiV1_8,
        encoder: &EncoderApi,
        input: &Pump<'_>,
    ) -> Result<Progress, &'static str> {
        if !input.capture_due {
            return if input.repeat_due
                && let Some(frame) = input.repeat_frame
            {
                repeat(encoder, frame, input)
            } else {
                Ok(Progress::default())
            };
        }
        let selection = input
            .configuration
            .gpu_processing
            .as_ref()
            .ok_or("resource_processor_selection_missing")?;
        if selection.service != self.service || !selection.enabled {
            return Err("resource_processor_selection_mismatch");
        }
        let api = unsafe { self.binding.table::<FrameResourceProcessorApi>() }
            .map_err(|_| "resource_processor_interface")?;
        let session = match self.sessions.entry(input.video) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                let target = target(input.configuration)?;
                let source = SourceFormat {
                    bit_depth: if selection.source_hdr_required {
                        10
                    } else {
                        input.configuration.video_stream_format.bit_depth
                    },
                    dynamic_range: if selection.source_hdr_required {
                        DynamicRange::Hdr
                    } else {
                        DynamicRange::Sdr
                    },
                };
                // SAFETY: this owning Processor retains its binding through
                // session cancellation/drop; capture remains live this call.
                entry.insert(
                    unsafe {
                        CpuResourceProcessingSession::new(
                            api,
                            input.capture,
                            input.video,
                            source,
                            target,
                        )
                    }
                    .map_err(|_| "resource_processor_configuration_invalid")?,
                )
            }
        };
        let mut submission = Submission {
            api: encoder,
            processor: None,
            video: input.video,
            encoder: input.encoder,
            configuration: input.configuration,
            status: MEDIA_PENDING,
            called: false,
            allow_resize: false,
            track_wgc_arrival: input.provider == CaptureProvider::Wgc,
            capture_reused: false,
            capture_arrival: None,
            force_idr: input.force_idr,
            timestamp_us: input.timestamp_us,
            sequence: input.sequence,
            frame: None,
            owned_frame: None,
            encoder_submit_us: 0,
            frame_copy_us: 0,
        };
        let started = Instant::now();
        let mut delivered = 0;
        // SAFETY: authenticated capture/processor bindings and their sessions
        // stay live; the SDK validates CPU-v2 output before this synchronous use.
        let status = unsafe {
            session.forward(capture, |frame| {
                delivered += 1;
                let cpu = &*frame.resource.data.cast::<CpuFrame>();
                submit_frame(
                    &mut submission,
                    cpu,
                    Some(frame.freshness == CAPTURE_FRAME_FRESH),
                    Some(frame.capture_pacing_timestamp_us),
                )
            })
        };
        let status = match status {
            Ok(status) => status,
            Err(amaterasu_module_sdk::capture::ConnectionError::Native(
                MEDIA_BUSY | MEDIA_PENDING,
            )) if !session.is_retired() => MEDIA_BUSY,
            Err(_) => return Err("resource_processor_negotiation_failed"),
        };
        if !matches!(status, STATUS_OK | MEDIA_BUSY | MEDIA_PENDING) {
            return Err("resource_processor_failed");
        }
        if submission.called && !matches!(submission.status, STATUS_OK | MEDIA_BUSY) {
            return Err("frame_rejected");
        }
        Ok(Progress {
            submitted: submission.called && submission.status == STATUS_OK,
            dropped: submission.called && submission.status == MEDIA_BUSY,
            capture_deferred: !submission.called && matches!(status, MEDIA_BUSY | MEDIA_PENDING),
            capture_reused: submission.capture_reused,
            capture_arrival: submission.capture_arrival,
            frame: submission.frame,
            owned_frame: submission.owned_frame,
            capture_callback_us: elapsed_micros(started),
            capture_callback_count: 1,
            capture_frame_count: delivered,
            encoder_submit_us: submission.encoder_submit_us,
            frame_copy_us: submission.frame_copy_us,
            ..Progress::default()
        })
    }
}
impl Drop for Processor {
    fn drop(&mut self) {
        self.finish_all();
    }
}
#[cfg(test)]
mod tests;

fn target(configuration: &Configuration) -> Result<CpuProcessingTarget, &'static str> {
    let hdr = configuration.video_stream_format.dynamic_range == DynamicRange::Hdr;
    let color = |rgb: bool, full: bool| FrameColorMetadata {
        version: 1,
        size: size_of::<FrameColorMetadata>() as u32,
        primaries: if hdr {
            FRAME_PRIMARIES_BT2020
        } else {
            FRAME_PRIMARIES_BT709
        },
        transfer: if hdr {
            FRAME_TRANSFER_PQ
        } else if rgb {
            FRAME_TRANSFER_SRGB
        } else {
            FRAME_TRANSFER_BT709
        },
        matrix: if hdr {
            FRAME_MATRIX_BT2020_NCL
        } else if rgb {
            FRAME_MATRIX_RGB
        } else {
            FRAME_MATRIX_BT709
        },
        range: if full {
            FRAME_RANGE_FULL
        } else {
            FRAME_RANGE_LIMITED
        },
    };
    let mut formats = Vec::new();
    if configuration.video_stream_format.bit_depth == 10 {
        if !matches!(
            configuration.color_mode,
            ColorMode::Bt709Limited | ColorMode::Bt709Full
        ) || hdr && !configuration.color_mode.is_default()
        {
            return Err("resource_processor_output_color_unsupported");
        }
        formats.push(CpuSourceFormat {
            pixel_format: PIXEL_P010,
            color: color(false, configuration.color_mode == ColorMode::Bt709Full),
        });
    } else if !hdr {
        if configuration.color_mode.is_default() {
            formats.push(CpuSourceFormat {
                pixel_format: PIXEL_NV12,
                color: color(false, false),
            });
        }
        formats.push(CpuSourceFormat {
            pixel_format: PIXEL_BGRA8,
            color: color(true, true),
        });
    }
    Ok(CpuProcessingTarget {
        width: configuration.width,
        height: configuration.height,
        formats,
    })
}
