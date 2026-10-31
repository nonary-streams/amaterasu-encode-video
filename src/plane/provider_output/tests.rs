use super::*;
use amaterasu_module_sdk::abi::{AbiHeader, BorrowedBytes, NativeInterface};
use std::ptr;

fn id(value: u64) -> EncoderFormatId {
    EncoderFormatId {
        namespace_id: ENCODER_FORMAT_NAMESPACE_STANDARD,
        value,
        schema: ENCODER_FORMAT_SCHEMA_V1,
    }
}
fn config() -> EncoderSessionConfig {
    EncoderSessionConfig {
        size: size_of::<EncoderSessionConfig>() as u32,
        mode: ENCODER_SESSION_SOURCE,
        codec: id(CODEC_H264 as u64),
        framing: id(ENCODER_FRAMING_ANNEX_B),
        source_id: 5,
        config_generation: 7,
        options_schema: id(0),
        options: BorrowedBytes {
            ptr: ptr::null(),
            len: 0,
        },
    }
}
fn chunk(bytes: &[u8]) -> EncoderOutputChunk {
    EncoderOutputChunk {
        size: size_of::<EncoderOutputChunk>() as u32,
        flags: ENCODER_CHUNK_ACCESS_UNIT_START | ENCODER_CHUNK_ACCESS_UNIT_END,
        session_id: 4,
        access_unit_id: 1,
        submission_id: 0,
        source_id: 5,
        config_generation: 7,
        timestamp_us: 8,
        codec: id(CODEC_H264 as u64),
        framing: id(ENCODER_FRAMING_ANNEX_B),
        bytes: BorrowedBytes {
            ptr: bytes.as_ptr(),
            len: bytes.len(),
        },
    }
}
struct Mock {
    chunk: EncoderOutputChunk,
    calls: usize,
    repeat: bool,
    no_callback: bool,
}
unsafe extern "C" fn receive(
    context: *mut c_void,
    _: u64,
    _: usize,
    sink: EncoderOutputSink,
) -> Status {
    // SAFETY: tests keep the mock alive through the synchronous call.
    let mock = unsafe { &mut *context.cast::<Mock>() };
    if mock.no_callback {
        return STATUS_OK;
    }
    mock.calls += 1;
    // SAFETY: the chunk and sink context remain alive for the callback.
    let status = unsafe { (sink.write)(sink.context, &mock.chunk) };
    if mock.repeat {
        // SAFETY: same synchronous borrow; this deliberately violates the provider contract.
        return unsafe { (sink.write)(sink.context, &mock.chunk) };
    }
    status
}
unsafe extern "C" fn probe(
    _: *mut c_void,
    _: *const EncoderSessionConfig,
    _: EncoderControlSink,
) -> Status {
    STATUS_OK
}
unsafe extern "C" fn configure(_: *mut c_void, _: u64, _: *const EncoderSessionConfig) -> Status {
    STATUS_OK
}
unsafe extern "C" fn control(
    _: *mut c_void,
    _: u64,
    _: *const EncoderControlRequest,
    _: EncoderControlSink,
) -> Status {
    STATUS_OK
}
unsafe extern "C" fn drain(_: *mut c_void, _: u64) -> Status {
    STATUS_OK
}
unsafe extern "C" fn destroy(_: *mut c_void, _: u64) {}
fn api(mock: &mut Mock) -> EncoderProviderApi {
    EncoderProviderApi {
        base: NativeInterface {
            header: AbiHeader {
                size: ENCODER_PROVIDER_API_V1_0_SIZE,
                major: 1,
                minor: 0,
            },
            context: (mock as *mut Mock).cast(),
            release: None,
        },
        probe,
        configure_session: configure,
        receive,
        control,
        subscribe: None,
        unsubscribe: None,
        drain,
        destroy_session: destroy,
    }
}
unsafe extern "C" fn accept(_: *mut c_void, _: *const EncodedPacket) -> Status {
    STATUS_OK
}
fn pop(queues: &mut Queues) -> Status {
    // SAFETY: this sink does not retain the packet or its bytes.
    unsafe {
        queues.receive(
            "alice",
            9,
            PacketSink {
                context: ptr::null_mut(),
                write: accept,
            },
        )
    }
}
#[test]
fn backpressure_retries_same_chunk_and_eos_is_once() {
    let data = [1];
    let mut mock = Mock {
        chunk: chunk(&data),
        calls: 0,
        repeat: false,
        no_callback: false,
    };
    let api = api(&mut mock);
    let mut output = ProviderOutput::new(4, 9, &config()).unwrap();
    let mut queues = Queues::default();
    queues.open(9, "alice", 0);
    assert_eq!(unsafe { output.drain(&api) }, STATUS_OK);
    for id in 1..=4 {
        mock.chunk.access_unit_id = id;
        mock.chunk.timestamp_us = id;
        std::hint::black_box(&mock);
        assert_eq!(unsafe { output.receive(&api, &mut queues) }, STATUS_OK);
    }
    mock.chunk.access_unit_id = 5;
    mock.chunk.timestamp_us = 5;
    std::hint::black_box(&mock);
    assert_eq!(unsafe { output.receive(&api, &mut queues) }, MEDIA_BUSY);
    assert_eq!(pop(&mut queues), STATUS_OK);
    std::hint::black_box(&mock);
    assert_eq!(unsafe { output.receive(&api, &mut queues) }, STATUS_OK);
    mock.chunk.flags = ENCODER_CHUNK_END_OF_STREAM;
    mock.chunk.bytes.len = 0;
    std::hint::black_box(&mock);
    assert_eq!(unsafe { output.receive(&api, &mut queues) }, MEDIA_BUSY);
    assert_eq!(pop(&mut queues), STATUS_OK);
    std::hint::black_box(&mock);
    assert_eq!(unsafe { output.receive(&api, &mut queues) }, STATUS_OK);
    std::hint::black_box(&mock);
    assert_eq!(unsafe { output.receive(&api, &mut queues) }, MEDIA_CLOSED);
    for _ in 0..4 {
        assert_eq!(pop(&mut queues), STATUS_OK);
    }
    assert_eq!(pop(&mut queues), MEDIA_CLOSED);
}
#[test]
fn rejects_foreign_interleaved_and_bad_negotiation() {
    let data = [1];
    let mut mock = Mock {
        chunk: chunk(&data),
        calls: 0,
        repeat: false,
        no_callback: false,
    };
    let api = api(&mut mock);
    let mut queues = Queues::default();
    queues.open(9, "alice", 0);
    let mut output = ProviderOutput::new(4, 9, &config()).unwrap();
    mock.chunk.session_id = 8;
    assert_eq!(
        unsafe { output.receive(&api, &mut queues) },
        STATUS_INVALID_ARGUMENT
    );
    mock.chunk.session_id = 4;
    mock.chunk.flags = ENCODER_CHUNK_ACCESS_UNIT_START;
    std::hint::black_box(&mock);
    assert_eq!(unsafe { output.receive(&api, &mut queues) }, STATUS_OK);
    mock.chunk.access_unit_id = 2;
    mock.chunk.flags = ENCODER_CHUNK_ACCESS_UNIT_END;
    assert_eq!(
        unsafe { output.receive(&api, &mut queues) },
        STATUS_INVALID_ARGUMENT
    );
    mock.chunk.access_unit_id = 1;
    std::hint::black_box(&mock);
    assert_eq!(unsafe { output.receive(&api, &mut queues) }, STATUS_OK);
    let mut bad = config();
    bad.codec = id(999);
    assert!(ProviderOutput::new(4, 9, &bad).is_err());
    bad = config();
    bad.framing = id(ENCODER_FRAMING_LENGTH_PREFIXED);
    assert!(ProviderOutput::new(4, 9, &bad).is_err());
}
#[test]
fn detects_bad_callback_contract_and_stops() {
    let data = [1];
    let mut mock = Mock {
        chunk: chunk(&data),
        calls: 0,
        repeat: false,
        no_callback: true,
    };
    let api = api(&mut mock);
    let mut queues = Queues::default();
    queues.open(9, "alice", 0);
    let mut output = ProviderOutput::new(4, 9, &config()).unwrap();
    assert_eq!(
        unsafe { output.receive(&api, &mut queues) },
        STATUS_INVALID_ARGUMENT
    );
    mock.no_callback = false;
    std::hint::black_box(&mock);
    assert_eq!(
        unsafe { output.receive(&api, &mut queues) },
        STATUS_INVALID_ARGUMENT
    );
    let mut output = ProviderOutput::new(4, 9, &config()).unwrap();
    mock.repeat = true;
    std::hint::black_box(&mock);
    assert_eq!(
        unsafe { output.receive(&api, &mut queues) },
        STATUS_INVALID_ARGUMENT
    );
    assert_eq!(
        unsafe { output.receive(&api, &mut queues) },
        STATUS_INVALID_ARGUMENT
    );
}
