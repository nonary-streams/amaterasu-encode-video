use crate::queue::Queues;
use amaterasu_media::{MEDIA_BUSY, MEDIA_UNSUPPORTED, PacketSink, VideoApiV1_1};
use amaterasu_module_sdk::abi::*;
use std::{
    ffi::c_void,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex},
};
pub struct Factory {
    pub api: NativeServiceFactory,
    queues: Arc<Mutex<Queues>>,
}

impl Factory {
    pub fn new(queues: Arc<Mutex<Queues>>) -> Box<Self> {
        let mut factory = Box::new(Self {
            api: NativeServiceFactory {
                header: AbiHeader::of::<NativeServiceFactory>(),
                context: std::ptr::null_mut(),
                bind: Some(bind),
            },
            queues,
        });
        factory.api.context = (&mut *factory as *mut Self).cast();
        factory
    }
}

struct Binding {
    api: VideoApiV1_1,
    queues: Arc<Mutex<Queues>>,
    caller: String,
}

unsafe extern "C" fn bind(
    context: *mut c_void,
    caller: BorrowedBytes,
    query: *const NativeInterfaceQuery,
    output: *mut *const NativeInterface,
) -> Status {
    guarded(|| {
        if context.is_null() || query.is_null() || output.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: runtime provides live query/output and retains boxed factory.
        unsafe {
            *output = std::ptr::null();
        }
        let query = unsafe { &*query };
        if !query.header.supports::<NativeInterfaceQuery>()
            || query.interface_major != 1
            || query.min_minor > 1
            || query.min_size as usize > size_of::<VideoApiV1_1>()
        {
            return MEDIA_UNSUPPORTED;
        }
        // SAFETY: immutable runtime principal is borrowed for this factory call.
        let caller = match unsafe { caller.read() }
            .ok()
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
        {
            Some(caller) if !caller.is_empty() => caller.to_owned(),
            _ => return STATUS_INVALID_ARGUMENT,
        };
        let factory = unsafe { &*context.cast::<Factory>() };
        let mut binding = Box::new(Binding {
            api: VideoApiV1_1 {
                base: NativeInterface {
                    header: AbiHeader {
                        size: size_of::<VideoApiV1_1>() as u32,
                        major: 1,
                        minor: 1,
                    },
                    context: std::ptr::null_mut(),
                    release: Some(release),
                },
                receive,
                geometry,
            },
            queues: factory.queues.clone(),
            caller,
        });
        binding.api.base.context = (&mut *binding as *mut Binding).cast();
        let table = &binding.api.base as *const NativeInterface;
        let _ = Box::into_raw(binding);
        // SAFETY: transfers provider-owned table until exactly one release.
        unsafe {
            *output = table;
        }
        STATUS_OK
    })
}

unsafe extern "C" fn release(context: *mut c_void) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !context.is_null() {
            // SAFETY: runtime releases this unique binding after all calls stop.
            unsafe {
                drop(Box::from_raw(context.cast::<Binding>()));
            }
        }
    }));
}

unsafe extern "C" fn receive(context: *mut c_void, stream: u64, sink: PacketSink) -> Status {
    guarded(|| {
        if context.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: binding and sink remain live for this synchronous call.
        let binding = unsafe { &*context.cast::<Binding>() };
        let Ok(mut queues) = binding.queues.try_lock() else {
            return MEDIA_BUSY;
        };
        unsafe { queues.receive(&binding.caller, stream, sink) }
    })
}

unsafe extern "C" fn geometry(
    context: *mut c_void,
    stream: u64,
    output: *mut amaterasu_media::CaptureGeometry,
) -> Status {
    guarded(|| {
        if context.is_null()
            || output.is_null()
            || !(output as usize).is_multiple_of(align_of::<amaterasu_media::CaptureGeometry>())
        {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: the broker retains this binding and aligned output slot until
        // return; copied queue metadata never borrows a capture provider.
        let binding = unsafe { &*context.cast::<Binding>() };
        let Ok(queues) = binding.queues.try_lock() else {
            return MEDIA_BUSY;
        };
        match queues.geometry(&binding.caller, stream) {
            Ok(Some(geometry)) => {
                unsafe { output.write(geometry) };
                STATUS_OK
            }
            Ok(None) => MEDIA_UNSUPPORTED,
            Err(status) => status,
        }
    })
}

fn guarded(operation: impl FnOnce() -> Status) -> Status {
    catch_unwind(AssertUnwindSafe(operation)).unwrap_or(STATUS_INTERNAL_ERROR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use amaterasu_media::{CODEC_H264, EncodedPacket, MEDIA_CLOSED, VideoApi};
    unsafe extern "C" fn accept(_: *mut c_void, _: *const EncodedPacket) -> Status {
        STATUS_OK
    }
    #[test]
    fn factory_copies_principal_and_binds_packet_ownership() {
        let queues = Arc::new(Mutex::new(Queues::default()));
        queues.lock().unwrap().open(7, "alice", 0);
        let bytes = [1];
        let packet = EncodedPacket {
            codec: CODEC_H264,
            flags: 0,
            data: bytes.as_ptr(),
            len: 1,
            timestamp_us: 1,
            sequence: 1,
        };
        assert_eq!(
            unsafe { queues.lock().unwrap().push(7, &packet) },
            STATUS_OK
        );
        let factory = Factory::new(queues);
        let query = NativeInterfaceQuery {
            header: AbiHeader::of::<NativeInterfaceQuery>(),
            interface_major: 1,
            min_minor: 0,
            min_size: size_of::<VideoApi>() as u32,
        };
        for (caller, expected) in [("bob", MEDIA_CLOSED), ("alice", STATUS_OK)] {
            let mut output = std::ptr::null();
            assert_eq!(
                unsafe {
                    bind(
                        factory.api.context,
                        BorrowedBytes::new(caller.as_bytes()),
                        &query,
                        &mut output,
                    )
                },
                STATUS_OK
            );
            // SAFETY: factory returns the exact VideoApi table until release.
            let api = unsafe { &*output.cast::<VideoApi>() };
            assert_eq!(
                unsafe {
                    (api.receive)(
                        api.base.context,
                        7,
                        PacketSink {
                            context: std::ptr::null_mut(),
                            write: accept,
                        },
                    )
                },
                expected
            );
            unsafe { (api.base.release.unwrap())(api.base.context) };
        }
    }
}
