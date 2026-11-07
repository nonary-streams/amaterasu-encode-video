//! Isolated provider substitutions: a packet borrower must not block host polls.
use super::*;
use amaterasu_module_sdk::{
    Command, Response,
    abi::*,
    encode::{
        ControlRequest, ControlResponse, EncoderProvider, OutputQueue, ProviderAdapter,
        SessionConfig, SessionMode,
    },
};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Provider {
    output: OutputQueue,
    controls: Arc<AtomicUsize>,
}

fn applied(request: u64) -> ControlResponse {
    ControlResponse {
        kind: 0,
        request_id: request,
        effective_generation: 1,
        effective_submission_id: 0,
        outcome: ENCODER_OUTCOME_APPLIED,
        schema: Default::default(),
        data: Vec::new(),
    }
}

impl EncoderProvider for Provider {
    fn probe(&self, _: SessionConfig) -> Result<ControlResponse, Status> {
        Ok(applied(0))
    }
    fn configure(&self, _: u64, _: SessionConfig) -> Result<(), Status> {
        Ok(())
    }
    fn output(&self, _: u64) -> Result<OutputQueue, Status> {
        Ok(self.output.clone())
    }
    fn control(&self, _: u64, request: ControlRequest) -> Result<ControlResponse, Status> {
        self.controls.fetch_add(1, Ordering::Relaxed);
        Ok(applied(request.request_id))
    }
    fn drain(&self, _: u64) -> Result<(), Status> {
        Ok(())
    }
    fn destroy(&self, _: u64) {}
    fn release(&self) -> Result<(), Status> {
        Ok(())
    }
}

struct Host {
    api: *const EncoderProviderApi,
}

unsafe extern "C" fn invoke(context: *mut c_void, input: BorrowedBytes, sink: ReplySink) -> Status {
    // SAFETY: the test retains the endpoint, command bytes and provider binding.
    let host = unsafe { &*context.cast::<Host>() };
    let command: Command = serde_json::from_slice(unsafe { input.read() }.unwrap()).unwrap();
    let response = match command {
        Command::NativeBindTyped { contract, .. } => {
            assert_eq!(contract, native_contracts::ENCODER_PROVIDER);
            Response::NativeBound {
                binding: 1,
                address: host.api as usize,
            }
        }
        Command::NativeRelease { binding: 1 } => {
            let api = unsafe { &*host.api };
            // SAFETY: the plane releases its unique adapter after Source.finish.
            unsafe { (api.base.release.unwrap())(api.base.context) };
            Response::Ok
        }
        _ => panic!("unexpected fixture command: {command:?}"),
    };
    unsafe { sink.send(&serde_json::to_vec(&response).unwrap()) };
    STATUS_OK
}

fn plane(host: &mut Host, queues: Arc<Mutex<Queues>>) -> NativePlane {
    let endpoint = HostApi {
        header: AbiHeader::of::<HostApi>(),
        context: (host as *mut Host).cast(),
        invoke,
    };
    // SAFETY: each test drops the plane before the host or provider context.
    let runtime = unsafe { RuntimeClient::from_endpoint(endpoint) };
    NativePlane::bind(
        &runtime,
        queues,
        &BTreeSet::new(),
        &BTreeMap::new(),
        &[(
            "fixture".into(),
            crate::model::SourceEncoderRegistration {
                control_service: "fixture.encode/1".into(),
                native_service: "fixture.source/1".into(),
                source_ids: [42].into(),
            },
        )]
        .into(),
        &BTreeMap::new(),
        None,
        None,
    )
    .unwrap()
}

fn borrow_queue(
    queues: Arc<Mutex<Queues>>,
) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<bool>) {
    let (ready, started) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let borrower = std::thread::spawn(move || {
        let _guard = queues.lock().unwrap();
        ready.send(()).unwrap();
        // A blocking baseline also exits; success requires release by the caller.
        released
            .recv_timeout(std::time::Duration::from_secs(1))
            .is_ok()
    });
    started.recv().unwrap();
    (release, borrower)
}

fn host(controls: Arc<AtomicUsize>) -> Host {
    Host {
        api: ProviderAdapter::new(Provider {
            output: OutputQueue::new(SessionMode::Source, 4, 1024).unwrap(),
            controls,
        })
        .into_binding(),
    }
}

#[test]
fn source_output_queue_backpressure_defers_idr_without_blocking_the_host() {
    let controls = Arc::new(AtomicUsize::new(0));
    let mut host = host(controls.clone());
    let queues = Arc::new(Mutex::new(Queues::default()));
    let mut plane = plane(&mut host, queues.clone());
    plane.select_encoder("fixture.encode/1");
    let configuration: Configuration = serde_json::from_value(
        serde_json::json!({"width":16,"height":16,"fps":60,"bitrate":1_000_000}),
    )
    .unwrap();
    plane
        .start_source(1, 2, 42, &configuration, 1000, [1; 16])
        .unwrap();
    let (release, borrower) = borrow_queue(queues);
    let result = plane.pump_source(1, 2, true);
    let controls_while_borrowed = controls.load(Ordering::Relaxed);
    let _ = release.send(());
    let released_by_caller = borrower.join().unwrap();
    assert!(
        released_by_caller,
        "source poll waited on a packet callback"
    );
    assert_eq!(result, Ok(false));
    assert_eq!(controls_while_borrowed, 0);
    assert_eq!(plane.pump_source(1, 2, true), Ok(true));
    assert_eq!(controls.load(Ordering::Relaxed), 1);
    plane.finish_stream(1);
}

unsafe extern "C" fn submit(_: *mut c_void, _: u64, _: *const CpuFrame, _: u32) -> Status {
    STATUS_OK
}
unsafe extern "C" fn receive(_: *mut c_void, _: u64, _: PacketSink) -> Status {
    MEDIA_PENDING
}
unsafe extern "C" fn drain(_: *mut c_void, _: u64) -> Status {
    STATUS_OK
}

#[test]
fn optional_encoder_timing_does_not_wait_for_a_packet_borrower() {
    let mut host = host(Arc::new(AtomicUsize::new(0)));
    let queues = Arc::new(Mutex::new(Queues::default()));
    let plane = plane(&mut host, queues.clone());
    let encoder = EncoderApi {
        base: NativeInterface {
            header: AbiHeader::of::<EncoderApi>(),
            context: std::ptr::null_mut(),
            release: None,
        },
        submit,
        receive,
        drain,
    };
    let configuration: Configuration = serde_json::from_value(
        serde_json::json!({"width":16,"height":16,"fps":60,"bitrate":1_000_000}),
    )
    .unwrap();
    let input = Pump {
        video: 1,
        provider: CaptureProvider::Dxgi,
        frame_path: FramePath::Cpu,
        capture: 3,
        encoder: 2,
        configuration: &configuration,
        capture_due: true,
        constant_rate: false,
        repeat_due: false,
        repeat_frame: None,
        timestamp_us: 0,
        sequence: 1,
        force_idr: false,
    };
    let (release, borrower) = borrow_queue(queues);
    let result = plane.receive_packets(
        &encoder,
        &input,
        Progress {
            submitted: true,
            ..Progress::default()
        },
    );
    let _ = release.send(());
    let released_by_caller = borrower.join().unwrap();
    assert!(
        released_by_caller,
        "optional timing waited on a packet callback"
    );
    assert!(result.unwrap().submitted);
}
