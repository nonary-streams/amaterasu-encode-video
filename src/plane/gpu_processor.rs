//! Synchronous processing of immutable GPU frame leases. Every retained lease
//! is released before the native bindings can be destroyed.
use amaterasu_media::{CaptureResourceFrame, DmaBufFrameLease, GpuProcessorApi};
pub(super) use amaterasu_module_sdk::processing::ScopedGpuFrame as ScopedLease;
use amaterasu_module_sdk::{
    NativeBinding,
    abi::{STATUS_OK, Status},
};

/// Read only an advertised additive tail. An older processor has unspecified
/// output metadata; never substitute capture metadata for transformed pixels.
pub fn hdr_metadata(
    binding: &NativeBinding,
    stream: u64,
) -> Result<Option<amaterasu_media::HdrMetadata>, &'static str> {
    use amaterasu_media::*;
    // SAFETY: typed binding identifies the GPU processor family and pins its table.
    let api = unsafe { table(binding) }?;
    if api.base.header.minor < GPU_PROCESSOR_METADATA_INTERFACE_MINOR
        || api.base.header.size < GPU_PROCESSOR_API_V1_1_SIZE
    {
        return Ok(None);
    }
    let api =
        unsafe { binding.table::<GpuProcessorApiV1_1>() }.map_err(|_| "gpu_processor_interface")?;
    let mut metadata = HdrMetadata::default();
    // SAFETY: the binding and writable output are live throughout this query.
    match unsafe { (api.hdr_metadata)(binding.context(), stream, &mut metadata) } {
        STATUS_OK if metadata.enabled <= 1 && metadata.reserved == 0 => Ok(Some(metadata)),
        MEDIA_UNSUPPORTED => Ok(None),
        _ => Err("processor_hdr_metadata_unavailable"),
    }
}

/// # Safety
/// The binding must be a GPU processor interface and remain alive throughout
/// every use of the returned table.
pub unsafe fn table(binding: &NativeBinding) -> Result<&GpuProcessorApi, &'static str> {
    // SAFETY: caller identifies the interface; the binding checks size/alignment.
    let api =
        unsafe { binding.table::<GpuProcessorApi>() }.map_err(|_| "gpu_processor_interface")?;
    if api.base.header.major != amaterasu_media::GPU_PROCESSOR_INTERFACE_MAJOR {
        return Err("gpu_processor_interface");
    }
    Ok(api)
}

/// Like `dispatch`, but delivers the processor's negotiated resource output.
///
/// # Safety
/// As for `dispatch`; `handoff` must be the stream's current selection.
pub unsafe fn dispatch_to_resource<F>(
    api: &GpuProcessorApi,
    stream: u64,
    handoff: amaterasu_module_sdk::processing::ResourceHandoff,
    lease: &DmaBufFrameLease,
    freshness: u32,
    submit: F,
) -> super::processor::Outcome
where
    F: FnMut(&CaptureResourceFrame) -> Status,
{
    use amaterasu_module_sdk::processing::{ProcessingOutcome, process_gpu_to_resource};
    // SAFETY: caller supplies live input and provider bindings for this call.
    match unsafe { process_gpu_to_resource(api, stream, handoff, lease, freshness, submit) } {
        ProcessingOutcome::Accepted => super::processor::Outcome::Accepted,
        ProcessingOutcome::Busy => super::processor::Outcome::Busy,
        ProcessingOutcome::Failed(status) => super::processor::Outcome::Failed(status),
        ProcessingOutcome::ProtocolViolation => super::processor::Outcome::ProtocolViolation,
    }
}

/// Built-in video uses the same dispatch adapter available to external consumers.
pub unsafe fn dispatch<F>(
    api: &GpuProcessorApi,
    stream: u64,
    lease: &DmaBufFrameLease,
    freshness: u32,
    submit: F,
) -> super::processor::Outcome
where
    F: FnMut(&DmaBufFrameLease) -> Status,
{
    use amaterasu_module_sdk::processing::{ProcessingOutcome, process_gpu};
    // SAFETY: caller supplies live input and provider bindings for this call.
    match unsafe { process_gpu(api, stream, lease, freshness, submit) } {
        ProcessingOutcome::Accepted => super::processor::Outcome::Accepted,
        ProcessingOutcome::Busy => super::processor::Outcome::Busy,
        ProcessingOutcome::Failed(status) => super::processor::Outcome::Failed(status),
        ProcessingOutcome::ProtocolViolation => super::processor::Outcome::ProtocolViolation,
    }
}
