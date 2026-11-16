//! Typed negotiated-unit delivery over the video module's owned queue.
use crate::queue::Queues;
use amaterasu_media::{MEDIA_BUSY, MEDIA_UNSUPPORTED, NegotiatedVideoApi, NegotiatedVideoUnitSink};
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
    api: NegotiatedVideoApi,
    queues: Arc<Mutex<Queues>>,
    owner: String,
}

unsafe extern "C" fn bind(
    context: *mut c_void,
    principal: BorrowedBytes,
    query: *const NativeInterfaceQuery,
    output: *mut *const NativeInterface,
) -> Status {
    catch_unwind(AssertUnwindSafe(|| {
        if context.is_null()
            || query.is_null()
            || output.is_null()
            || !(query as usize).is_multiple_of(align_of::<NativeInterfaceQuery>())
            || !(output as usize).is_multiple_of(align_of::<*const NativeInterface>())
        {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: runtime supplies live query, principal and writable output.
        unsafe {
            *output = std::ptr::null();
        }
        let query = unsafe { &*query };
        if !query.header.supports::<NativeInterfaceQuery>()
            || query.interface_major != 1
            || query.min_minor > 0
            || query.min_size as usize > size_of::<NegotiatedVideoApi>()
        {
            return MEDIA_UNSUPPORTED;
        }
        let owner = match unsafe { principal.read() }
            .ok()
            .and_then(|b| std::str::from_utf8(b).ok())
        {
            Some(owner) if !owner.is_empty() && owner.len() <= 128 => owner.to_owned(),
            _ => return STATUS_INVALID_ARGUMENT,
        };
        // SAFETY: factory remains registered until all bindings retire.
        let factory = unsafe { &*context.cast::<Factory>() };
        let mut binding = Box::new(Binding {
            api: NegotiatedVideoApi {
                base: NativeInterface {
                    header: AbiHeader {
                        size: size_of::<NegotiatedVideoApi>() as u32,
                        major: 1,
                        minor: 0,
                    },
                    context: std::ptr::null_mut(),
                    release: Some(release),
                },
                receive,
            },
            queues: factory.queues.clone(),
            owner,
        });
        binding.api.base.context = (&mut *binding as *mut Binding).cast();
        let binding = Box::into_raw(binding);
        // SAFETY: table belongs to the transferred binding until release.
        unsafe {
            *output = &(*binding).api.base;
        }
        STATUS_OK
    }))
    .unwrap_or(STATUS_INTERNAL_ERROR)
}

unsafe extern "C" fn release(context: *mut c_void) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !context.is_null() {
            // SAFETY: runtime releases this unique binding after calls end.
            drop(unsafe { Box::from_raw(context.cast::<Binding>()) });
        }
    }));
}

unsafe extern "C" fn receive(
    context: *mut c_void,
    stream: u64,
    sink: NegotiatedVideoUnitSink,
) -> Status {
    catch_unwind(AssertUnwindSafe(|| {
        if context.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: runtime retains this binding through the call.
        let binding = unsafe { &*context.cast::<Binding>() };
        let Ok(mut queues) = binding.queues.try_lock() else {
            return MEDIA_BUSY;
        };
        // SAFETY: sink obeys the synchronous, non-reentrant contract.
        unsafe { queues.receive_unit(&binding.owner, stream, sink) }
    }))
    .unwrap_or(STATUS_INTERNAL_ERROR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::UnitSelection;
    use amaterasu_media::*;

    unsafe extern "C" fn sink(context: *mut c_void, unit: *const NegotiatedVideoUnit) -> Status {
        let captured = unsafe { &mut *context.cast::<Vec<u8>>() };
        let unit = unsafe { &*unit };
        captured.extend_from_slice(unsafe { unit.data.read() }.unwrap());
        STATUS_OK
    }

    #[test]
    fn typed_factory_delivers_only_the_owning_principals_configured_units() {
        let queues = Arc::new(Mutex::new(Queues::default()));
        let codec = EncoderFormatId {
            namespace_id: 7,
            value: 2,
            schema: 1,
        };
        let framing = EncoderFormatId {
            namespace_id: 7,
            value: 3,
            schema: 1,
        };
        let mut guard = queues.lock().unwrap();
        guard.open(8, "alice", 100);
        guard
            .configure_units(
                8,
                UnitSelection {
                    stream: 8,
                    generation: 8,
                    offer_id: 3,
                    source_id: 6,
                    config_generation: 1,
                    codec,
                    framing,
                    clock_epoch: [9; 16],
                },
            )
            .unwrap();
        let data = [5, 6];
        let chunk = EncoderOutputChunk {
            size: size_of::<EncoderOutputChunk>() as u32,
            flags: ENCODER_CHUNK_ACCESS_UNIT_START | ENCODER_CHUNK_ACCESS_UNIT_END,
            session_id: 1,
            access_unit_id: 1,
            submission_id: 1,
            source_id: 6,
            config_generation: 1,
            timestamp_us: 2,
            codec,
            framing,
            bytes: BorrowedBytes::new(&data),
        };
        assert_eq!(unsafe { guard.push_unit_chunk(8, &chunk) }, STATUS_OK);
        drop(guard);
        let factory = Factory::new(queues);
        let query = NativeInterfaceQuery {
            header: AbiHeader::of::<NativeInterfaceQuery>(),
            interface_major: 1,
            min_minor: 0,
            min_size: size_of::<NegotiatedVideoApi>() as u32,
        };
        for (owner, expected) in [("bob", MEDIA_CLOSED), ("alice", STATUS_OK)] {
            let mut table = std::ptr::null();
            assert_eq!(
                unsafe {
                    bind(
                        factory.api.context,
                        BorrowedBytes::new(owner.as_bytes()),
                        &query,
                        &mut table,
                    )
                },
                STATUS_OK
            );
            let api = unsafe { &*table.cast::<NegotiatedVideoApi>() };
            let mut output = Vec::new();
            assert_eq!(
                unsafe {
                    (api.receive)(
                        api.base.context,
                        8,
                        NegotiatedVideoUnitSink {
                            context: (&mut output as *mut Vec<u8>).cast(),
                            write: sink,
                        },
                    )
                },
                expected
            );
            if owner == "alice" {
                assert_eq!(output, data);
            } else {
                assert!(output.is_empty());
            }
            unsafe { (api.base.release.unwrap())(api.base.context) };
        }
    }
}
