//! Synchronous processing of immutable GPU frame leases. Every retained lease
//! is released before the native bindings can be destroyed.
use super::processor::Outcome;
use amaterasu_media::{DmaBufFrameLease, DmaBufLeaseSink, GpuProcessorApi, MEDIA_BUSY};
use amaterasu_module_sdk::{
    NativeBinding,
    abi::{STATUS_INVALID_ARGUMENT, STATUS_OK, Status},
};
use std::ffi::c_void;

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

/// A synchronous reference whose release necessarily occurs before the caller
/// can drop the native binding. This guard intentionally cannot escape a pump.
pub(super) struct ScopedLease<'a> {
    lease: &'a DmaBufFrameLease,
}

impl<'a> ScopedLease<'a> {
    /// # Safety
    /// The provider binding and borrowed owner must outlive this guard.
    pub unsafe fn retain(lease: &'a DmaBufFrameLease) -> Result<Self, Status> {
        lease.validate()?;
        // SAFETY: validation checked the callback; caller retains its binding.
        let status = unsafe { lease.retain.unwrap()(lease.context) };
        if status == STATUS_OK {
            Ok(Self { lease })
        } else {
            Err(status)
        }
    }

    pub fn descriptor(&self) -> &DmaBufFrameLease {
        self.lease
    }
}

impl Drop for ScopedLease<'_> {
    fn drop(&mut self) {
        // SAFETY: one successful retain is released while its binding is live;
        // no asynchronous GPU operation is owned by this synchronous guard.
        unsafe { self.lease.release.unwrap()(self.lease.context, -1) };
    }
}

struct Delivery<'a, F> {
    submit: &'a mut F,
    sequence: u64,
    timestamp_us: u64,
    freshness: u32,
    count: usize,
    result: Status,
    invalid: bool,
}

unsafe extern "C" fn deliver<F>(
    context: *mut c_void,
    lease: *const DmaBufFrameLease,
    freshness: u32,
) -> Status
where
    F: FnMut(&DmaBufFrameLease) -> Status,
{
    if context.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: dispatch owns this stack context for the entire synchronous call.
    let delivery = unsafe { &mut *context.cast::<Delivery<'_, F>>() };
    delivery.count += 1;
    if delivery.count != 1 || lease.is_null() {
        delivery.invalid = true;
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: the trusted provider keeps the descriptor live during its callback.
    let lease = unsafe { &*lease };
    if lease.validate().is_err()
        || lease.frame.sequence != delivery.sequence
        || lease.frame.timestamp_us != delivery.timestamp_us
        || freshness != delivery.freshness
    {
        delivery.invalid = true;
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: all provider bindings outlive this dispatch. Retention is local
    // and paired by Drop before returning to the provider.
    let retained = match unsafe { ScopedLease::retain(lease) } {
        Ok(retained) => retained,
        Err(status) => {
            delivery.result = status;
            return status;
        }
    };
    delivery.result = (delivery.submit)(retained.descriptor());
    delivery.result
}

/// Processes one GPU frame, allowing only one synchronous output with unchanged
/// capture identity. The downstream encoder retains its own reference before
/// returning if GPU reads will continue asynchronously.
///
/// # Safety
/// Input ownership callbacks and both native bindings must remain live for this
/// call. Neither this function nor its submit callback keeps a lease afterward.
pub unsafe fn dispatch<F>(
    api: &GpuProcessorApi,
    stream: u64,
    lease: &DmaBufFrameLease,
    freshness: u32,
    mut submit: F,
) -> Outcome
where
    F: FnMut(&DmaBufFrameLease) -> Status,
{
    if lease.validate().is_err() {
        return Outcome::Failed(STATUS_INVALID_ARGUMENT);
    }
    let mut delivery = Delivery {
        submit: &mut submit,
        sequence: lease.frame.sequence,
        timestamp_us: lease.frame.timestamp_us,
        freshness,
        count: 0,
        result: STATUS_OK,
        invalid: false,
    };
    let sink = DmaBufLeaseSink {
        context: (&mut delivery as *mut Delivery<'_, F>).cast(),
        write: deliver::<F>,
    };
    // SAFETY: lease, sink, callbacks and native bindings are live for the call.
    let status = unsafe { (api.process)(api.base.context, stream, lease, freshness, sink) };
    if delivery.invalid
        || delivery.count > 1
        || (delivery.count == 1 && status != delivery.result)
        || (delivery.count == 0 && status == STATUS_OK)
    {
        return Outcome::ProtocolViolation;
    }
    if status == STATUS_OK {
        Outcome::Accepted
    } else if status == MEDIA_BUSY {
        Outcome::Busy
    } else {
        Outcome::Failed(status)
    }
}
