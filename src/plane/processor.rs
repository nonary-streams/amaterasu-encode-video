//! Optional synchronous CPU frame processing. No GPU or asynchronous lease is implied.
use amaterasu_media::{CpuFrame, CpuProcessorApi, FrameSink, MEDIA_BUSY};
use amaterasu_module_sdk::{
    NativeBinding,
    abi::{STATUS_INVALID_ARGUMENT, STATUS_OK, Status},
};
use std::ffi::c_void;

/// Resolve a v1 processor table while the binding remains alive.
///
/// # Safety
/// The binding must identify the CPU processor interface, whose provider obeys
/// its ABI contract. The returned table cannot outlive the binding.
pub unsafe fn table(binding: &NativeBinding) -> Result<&CpuProcessorApi, &'static str> {
    // SAFETY: the caller identifies the provider and its ABI; table checks size and alignment.
    let api = unsafe { binding.table::<CpuProcessorApi>() }.map_err(|_| "processor_interface")?;
    if api.base.header.major != amaterasu_media::CPU_PROCESSOR_INTERFACE_MAJOR {
        return Err("processor_interface");
    }
    Ok(api)
}

/// Outcome of an attempted synchronous transform and downstream submission.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Accepted,
    Busy,
    Failed(Status),
    ProtocolViolation,
}

struct Delivery<'a, F> {
    submit: &'a mut F,
    sequence: u64,
    timestamp_us: u64,
    count: usize,
    result: Status,
    invalid: bool,
}

unsafe extern "C" fn deliver<F>(context: *mut c_void, frame: *const CpuFrame) -> Status
where
    F: FnMut(&CpuFrame) -> Status,
{
    if context.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: process receives this live stack context and must invoke synchronously.
    let delivery = unsafe { &mut *context.cast::<Delivery<'_, F>>() };
    delivery.count += 1;
    if delivery.count != 1 || frame.is_null() {
        delivery.invalid = true;
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: the processor promises a live descriptor and allocation during callback.
    let frame = unsafe { &*frame };
    if frame.validate().is_err()
        || frame.sequence != delivery.sequence
        || frame.timestamp_us != delivery.timestamp_us
    {
        delivery.invalid = true;
        return STATUS_INVALID_ARGUMENT;
    }
    delivery.result = (delivery.submit)(frame);
    delivery.result
}

/// Transform and synchronously submit the result. No pointer survives this call.
///
/// # Safety
/// The input allocation must be readable and immutable during the call; the
/// processor must obey the native interface's callback and pointer contracts.
/// `submit` must not retain the output descriptor or its bytes.
pub unsafe fn dispatch<F>(
    api: &CpuProcessorApi,
    stream: u64,
    frame: &CpuFrame,
    mut submit: F,
) -> Outcome
where
    F: FnMut(&CpuFrame) -> Status,
{
    if frame.validate().is_err() {
        return Outcome::Failed(STATUS_INVALID_ARGUMENT);
    }
    let mut delivery = Delivery {
        submit: &mut submit,
        sequence: frame.sequence,
        timestamp_us: frame.timestamp_us,
        count: 0,
        result: STATUS_OK,
        invalid: false,
    };
    let sink = FrameSink {
        context: (&mut delivery as *mut Delivery<'_, F>).cast(),
        write: deliver::<F>,
    };
    // SAFETY: input and stack sink stay live until process returns, per caller and ABI.
    let status = unsafe { (api.process)(api.base.context, stream, frame, sink) };
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

#[cfg(test)]
mod tests {
    use super::*;
    use amaterasu_media::{MEDIA_BACKEND_ERROR, PIXEL_BGRA8};
    use amaterasu_module_sdk::abi::{AbiHeader, NativeInterface};

    unsafe extern "C" fn process(
        context: *mut c_void,
        _: u64,
        frame: *const CpuFrame,
        sink: FrameSink,
    ) -> Status {
        // SAFETY: each test provides a live mode and input for this synchronous call.
        let mode = unsafe { *context.cast::<u8>() };
        match mode {
            0 => MEDIA_BUSY,
            1 => unsafe { (sink.write)(sink.context, frame) },
            2 => STATUS_OK,
            3 => {
                // SAFETY: both callbacks receive the live test frame and sink.
                unsafe {
                    (sink.write)(sink.context, frame);
                    (sink.write)(sink.context, frame)
                }
            }
            4 => {
                let mut altered = unsafe { *frame };
                altered.sequence += 1;
                unsafe { (sink.write)(sink.context, &altered) }
            }
            _ => MEDIA_BACKEND_ERROR,
        }
    }

    #[test]
    fn dispatch_enforces_delivery_and_propagates_backpressure() {
        let bytes = [1u8; 4];
        let frame = CpuFrame {
            width: 1,
            height: 1,
            format: PIXEL_BGRA8,
            stride: 4,
            data: bytes.as_ptr(),
            len: bytes.len(),
            timestamp_us: 4,
            sequence: 7,
        };
        let mut mode = 0u8;
        let api = CpuProcessorApi {
            base: NativeInterface {
                header: AbiHeader::of::<CpuProcessorApi>(),
                context: (&mut mode as *mut u8).cast(),
                release: None,
            },
            process,
        };
        let mut calls = 0;
        // SAFETY: the fixture and bytes outlive each synchronous dispatch.
        let busy = unsafe {
            dispatch(&api, 1, &frame, |_| {
                calls += 1;
                STATUS_OK
            })
        };
        assert_eq!(busy, Outcome::Busy);
        assert_eq!(calls, 0);
        mode = 1;
        // SAFETY: the fixture and bytes outlive each synchronous dispatch.
        assert_eq!(
            unsafe { dispatch(&api, 1, &frame, |_| MEDIA_BUSY) },
            Outcome::Busy
        );
        // SAFETY: the fixture and bytes outlive each synchronous dispatch.
        assert_eq!(
            unsafe { dispatch(&api, 1, &frame, |_| STATUS_OK) },
            Outcome::Accepted
        );
        for bad_mode in [2, 3, 4] {
            mode = bad_mode;
            // SAFETY: the deliberately malformed fixture still uses live pointers.
            assert_eq!(
                unsafe { dispatch(&api, 1, &frame, |_| STATUS_OK) },
                Outcome::ProtocolViolation
            );
        }
        // Keep the value observable to the compiler after the pointer-based fixture calls.
        assert_eq!(mode, 4);
    }
}
