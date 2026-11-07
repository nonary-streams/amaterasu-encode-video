use super::*;
use crate::worker::WorkerBackend;
use amaterasu_abi::{AbiHeader, BorrowedBytes, HostApi, ReplySink};
use amaterasu_module_sdk::{ManagedModule, RuntimeClient};
use std::{
    collections::BTreeSet,
    ffi::c_void,
    sync::{
        Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

#[derive(Default)]
struct BrokerState {
    incoming: VecDeque<amaterasu_module_sdk::Response>,
    live: BTreeSet<u64>,
    completed: Vec<(u64, Value)>,
    completion_errors: BTreeMap<u64, &'static str>,
}

pub(super) struct Broker(Box<Mutex<BrokerState>>);

unsafe extern "C" fn broker_dispatch(
    context: *mut c_void,
    input: BorrowedBytes,
    reply: ReplySink,
) -> Status {
    use amaterasu_module_sdk::{Command, Response};
    // SAFETY: Broker owns the stable mutex and runtime borrows bytes/sink only
    // for the synchronous invocation.
    let broker = unsafe { &*context.cast::<Mutex<BrokerState>>() };
    let command: Command = serde_json::from_slice(unsafe { input.read().unwrap() }).unwrap();
    let mut broker = broker.lock().unwrap();
    let response = match command {
        Command::Poll {} => broker.incoming.pop_front().unwrap_or(Response::Pending),
        Command::CheckRequest { request } if broker.live.contains(&request) => Response::Ok,
        Command::CheckRequest { .. } => Response::error("request_ended", "cancelled fixture"),
        Command::Complete { request, payload } => {
            if let Some(code) = broker.completion_errors.remove(&request) {
                if code == "request_ended" {
                    broker.live.remove(&request);
                }
                Response::error(code, "controlled completion failure")
            } else if broker.live.remove(&request) {
                broker.completed.push((request, payload));
                Response::Ok
            } else {
                Response::error("request_ended", "cancelled fixture")
            }
        }
        _ => panic!("unexpected lifecycle test command"),
    };
    unsafe { reply.send(&serde_json::to_vec(&response).unwrap()) };
    STATUS_OK
}

impl Broker {
    pub(super) fn attach(module: &mut crate::EncodeModule) -> Self {
        let broker = Self(Box::default());
        // SAFETY: the boxed fixture state remains live through module destruction.
        module.runtime = unsafe {
            RuntimeClient::from_endpoint(HostApi {
                header: AbiHeader::of::<HostApi>(),
                context: (&*broker.0 as *const Mutex<BrokerState>).cast_mut().cast(),
                invoke: broker_dispatch,
            })
        };
        broker
    }

    pub(super) fn request(&self, id: u64, caller: &str, payload: Value) {
        let mut broker = self.0.lock().unwrap();
        broker.live.insert(id);
        broker
            .incoming
            .push_back(amaterasu_module_sdk::Response::Request {
                id,
                caller: caller.into(),
                initiator: "test".into(),
                service: crate::SERVICE.into(),
                remaining_ms: 5_000,
                payload,
            });
    }

    pub(super) fn completed(&self) -> Vec<(u64, Value)> {
        self.0.lock().unwrap().completed.clone()
    }

    pub(super) fn cancel(&self, id: u64) {
        self.0.lock().unwrap().live.remove(&id);
    }

    fn fail_completion(&self, id: u64, code: &'static str) {
        self.0.lock().unwrap().completion_errors.insert(id, code);
    }
}

struct DeferredClose {
    ready: Arc<std::sync::atomic::AtomicBool>,
    close_calls: Arc<AtomicUsize>,
}

impl Backend for DeferredClose {
    fn ready_to_close(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }
    fn close(self: Box<Self>) -> Result<(), &'static str> {
        assert!(self.ready_to_close(), "native close would block here");
        self.close_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn submit(&mut self, _: Vec<u8>, _: i64, _: bool) -> Result<SubmitResult, &'static str> {
        Ok(SubmitResult::Accepted)
    }
    fn receive(&mut self) -> Result<ReceiveResult, &'static str> {
        Ok(ReceiveResult::Ready(None))
    }
    fn drain(&mut self) -> Result<bool, &'static str> {
        Ok(true)
    }
}

fn deferred_session() -> (
    Sessions,
    Arc<std::sync::atomic::AtomicBool>,
    Arc<AtomicUsize>,
) {
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut sessions = Sessions::new(true);
    sessions
        .insert(
            "alice",
            configuration(),
            Box::new(DeferredClose {
                ready: ready.clone(),
                close_calls: calls.clone(),
            }),
            false,
        )
        .unwrap();
    (sessions, ready, calls)
}

#[test]
fn cancelled_close_keeps_cleanup_owned_and_a_retry_gets_its_own_completion() {
    let (sessions, ready, calls) = deferred_session();
    let mut module = module(sessions);
    let broker = Broker::attach(&mut module);
    broker.request(17, "alice", json!({"action":"close","session_id":1}));
    module.tick().unwrap();
    broker.cancel(17);
    broker.request(99, "alice", json!({"action":"close","session_id":1}));
    module.tick().unwrap();
    assert!(broker.completed().is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    ready.store(true, Ordering::SeqCst);
    module.tick().unwrap();
    assert_eq!(broker.completed(), vec![(99, close_reply(1))]);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn cancelled_close_without_a_waiter_still_retires_the_owned_session() {
    let (sessions, ready, calls) = deferred_session();
    let mut module = module(sessions);
    let broker = Broker::attach(&mut module);
    broker.request(17, "alice", json!({"action":"close","session_id":1}));
    module.tick().unwrap();
    broker.cancel(17);
    module.tick().unwrap();
    ready.store(true, Ordering::SeqCst);
    module.tick().unwrap();
    assert!(broker.completed().is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(module.shared.lock().unwrap().values.is_empty());
}

#[test]
fn cancelled_close_retains_its_target_when_a_native_call_holds_the_state_lock() {
    let (sessions, ready, calls) = deferred_session();
    let mut module = module(sessions);
    let broker = Broker::attach(&mut module);
    let shared = module.shared.clone();
    let held_native_call = shared.lock().unwrap();
    broker.request(31, "alice", json!({"action":"close","session_id":1}));
    module.tick().unwrap();
    broker.cancel(31);
    module.tick().unwrap();
    drop(held_native_call);
    module.tick().unwrap();
    assert_eq!(
        module.shared.lock().unwrap().status("alice", 1).unwrap()["state"],
        "closing"
    );
    ready.store(true, Ordering::SeqCst);
    module.tick().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(broker.completed().is_empty());
}

#[test]
fn duplicate_close_waiters_share_retirement_and_failed_completion_does_not_repeat_it() {
    let (sessions, ready, calls) = deferred_session();
    let mut module = module(sessions);
    let broker = Broker::attach(&mut module);
    for id in [17, 19] {
        broker.request(id, "alice", json!({"action":"close","session_id":1}));
        module.tick().unwrap();
    }
    broker.fail_completion(17, "busy");
    ready.store(true, Ordering::SeqCst);
    assert!(module.tick().is_err());
    assert_eq!(broker.completed(), vec![(19, close_reply(1))]);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    module.tick().unwrap();
    assert_eq!(
        broker.completed(),
        vec![(19, close_reply(1)), (17, close_reply(1))]
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn close_waiter_capacity_is_bounded_without_discarding_accepted_cleanup() {
    let (sessions, ready, calls) = deferred_session();
    let mut module = module(sessions);
    let broker = Broker::attach(&mut module);
    for id in 1..=129 {
        broker.request(id, "alice", json!({"action":"close","session_id":1}));
        module.tick().unwrap();
    }
    assert_eq!(
        broker.completed(),
        vec![(129, json!({"ok":false,"code":"encoder_close_limit"}))]
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    for id in 1..=128 {
        broker.cancel(id);
    }
    ready.store(true, Ordering::SeqCst);
    module.tick().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(module.closing.is_empty());
}

#[test]
fn cancelled_unvalidated_close_targets_are_bounded_while_native_state_is_busy() {
    let (sessions, ready, calls) = deferred_session();
    let mut module = module(sessions);
    let broker = Broker::attach(&mut module);
    let shared = module.shared.clone();
    let (entered, native_started) = mpsc::channel();
    let (release, native_release) = mpsc::channel();
    let native_call = std::thread::spawn(move || {
        let _held_native_call = shared.lock().unwrap();
        entered.send(()).unwrap();
        // A broken implementation must fail this regression rather than leave
        // the suite permanently blocked while trying to acquire native state.
        let _ = native_release.recv_timeout(Duration::from_secs(3));
    });
    native_started.recv_timeout(Duration::from_secs(2)).unwrap();
    for request in 1..=129 {
        broker.request(
            request,
            "mallory",
            json!({"action":"close","session_id":request + 1000}),
        );
        module.tick().unwrap();
        broker.cancel(request);
        module.tick().unwrap();
    }
    assert_eq!(
        broker.completed(),
        vec![(129, json!({"ok":false,"code":"encoder_close_limit"}))]
    );
    let _ = release.send(());
    native_call.join().unwrap();
    module.tick().unwrap();
    assert!(module.closing.is_empty());
    assert_eq!(
        module.shared.lock().unwrap().status("alice", 1).unwrap()["state"],
        "open"
    );
    broker.request(130, "alice", json!({"action":"close","session_id":1}));
    module.tick().unwrap();
    ready.store(true, Ordering::SeqCst);
    module.tick().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(broker.completed().last(), Some(&(130, close_reply(1))));
}

#[test]
fn repeated_unkeyed_open_and_close_do_not_accumulate_retirement_receipts() {
    struct RepeatBackend(Arc<AtomicUsize>);
    impl crate::factory::Factory for RepeatBackend {
        fn enumerate(&self) -> Result<Vec<crate::model::EncoderDescription>, &'static str> {
            Ok(vec![])
        }
        fn open(
            &self,
            _: EncoderIdentity,
            _: Configuration,
        ) -> Result<Box<dyn Backend>, &'static str> {
            Ok(Box::new(DeferredClose {
                ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                close_calls: self.0.clone(),
            }))
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let mut module = module(Sessions::with_factory(
        true,
        Arc::new(RepeatBackend(calls.clone())),
    ));
    let broker = Broker::attach(&mut module);
    for cycle in 0..16 {
        broker.request(100 + cycle * 2, "alice", json!({"action":"open","encoder":{"clsid":"fixture","friendly_name":"fixture"},"configuration":configuration()}));
        module.tick().unwrap();
        assert_eq!(
            broker.completed().last().unwrap().1["session_id"],
            cycle + 1
        );
        broker.request(
            101 + cycle * 2,
            "alice",
            json!({"action":"close","session_id":cycle + 1}),
        );
        module.tick().unwrap();
        assert_eq!(
            broker.completed().last(),
            Some(&(101 + cycle * 2, close_reply(cycle + 1)))
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 16);
}

#[test]
fn cancelled_open_delivery_retains_background_rollback_without_an_inline_close() {
    struct OneBackend(Mutex<Option<Box<dyn Backend>>>);
    impl crate::factory::Factory for OneBackend {
        fn enumerate(&self) -> Result<Vec<crate::model::EncoderDescription>, &'static str> {
            Ok(vec![])
        }
        fn open(
            &self,
            _: EncoderIdentity,
            _: Configuration,
        ) -> Result<Box<dyn Backend>, &'static str> {
            Ok(self.0.lock().unwrap().take().unwrap())
        }
    }
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let sessions = Sessions::with_factory(
        true,
        Arc::new(OneBackend(Mutex::new(Some(Box::new(DeferredClose {
            ready: ready.clone(),
            close_calls: calls.clone(),
        }))))),
    );
    let mut module = module(sessions);
    let broker = Broker::attach(&mut module);
    let open = json!({"action":"open","encoder":{"clsid":"fixture","friendly_name":"fixture"},"configuration":configuration(),"operation_id":"cancelled-open"});
    broker.request(52, "alice", open.clone());
    broker.fail_completion(52, "request_ended");
    module.tick().unwrap();
    assert!(broker.completed().is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        module.shared.lock().unwrap().status("alice", 1).unwrap()["state"],
        "closing"
    );
    broker.request(53, "alice", open);
    module.tick().unwrap();
    let replay_reply = broker.completed();
    ready.store(true, Ordering::SeqCst);
    module.tick().unwrap();
    assert_eq!(
        replay_reply,
        vec![(53, json!({"ok":false,"code":"session_closing"}))]
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(module.closing.is_empty());
}

fn configuration() -> Configuration {
    serde_json::from_value(json!({
        "width":16,"height":16,"fps":60,"bitrate":1_000_000
    }))
    .unwrap()
}

struct FailedClose(Arc<AtomicUsize>);

impl Backend for FailedClose {
    fn close(self: Box<Self>) -> Result<(), &'static str> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err("controlled_native_cleanup_failure")
    }

    fn submit(&mut self, _: Vec<u8>, _: i64, _: bool) -> Result<SubmitResult, &'static str> {
        Ok(SubmitResult::Accepted)
    }

    fn receive(&mut self) -> Result<ReceiveResult, &'static str> {
        Ok(ReceiveResult::Ready(None))
    }

    fn drain(&mut self) -> Result<bool, &'static str> {
        Ok(true)
    }
}

#[test]
fn failed_native_close_is_reported_and_repeated_close_retains_its_exact_error() {
    let close_calls = Arc::new(AtomicUsize::new(0));
    let mut sessions = Sessions::new(true);
    sessions
        .insert(
            "alice",
            configuration(),
            Box::new(FailedClose(close_calls.clone())),
            false,
        )
        .unwrap();
    assert_eq!(
        sessions.close("alice", 1),
        Err("controlled_native_cleanup_failure")
    );
    assert_eq!(close_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        sessions.close("alice", 1),
        Err("controlled_native_cleanup_failure")
    );
    assert_eq!(
        sessions.status("alice", 1),
        Err("controlled_native_cleanup_failure")
    );
    assert_eq!(sessions.close("mallory", 1), Err("session_not_found"));
    assert_eq!(close_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn failed_cleanup_evidence_consumes_the_same_bounded_owner_and_global_capacity() {
    let mut sessions = Sessions::new(true);
    let identity = crate::model::EncoderIdentity {
        clsid: "controlled".into(),
        hardware_url: None,
        friendly_name: "fixture".into(),
    };
    for owner in ["alice", "bob", "carol", "dave"] {
        for _ in 0..2 {
            sessions
                .validate_open(owner, &identity, &configuration())
                .unwrap();
            sessions
                .insert(
                    owner,
                    configuration(),
                    Box::new(FailedClose(Arc::new(AtomicUsize::new(0)))),
                    false,
                )
                .unwrap();
            let id = sessions.latest_id();
            assert!(sessions.close(owner, id).is_err());
        }
        assert_eq!(
            sessions.validate_open(owner, &identity, &configuration()),
            Err("session_limit")
        );
    }
    assert_eq!(sessions.close_failures.len(), 8);
    assert_eq!(
        sessions.validate_open("new-owner", &identity, &configuration()),
        Err("session_limit")
    );
}

#[test]
fn failed_cleanup_keeps_the_original_open_operation_fenced() {
    let close_calls = Arc::new(AtomicUsize::new(0));
    let mut sessions = Sessions::new(true);
    let identity = crate::model::EncoderIdentity {
        clsid: "controlled".into(),
        hardware_url: None,
        friendly_name: "fixture".into(),
    };
    sessions
        .insert(
            "alice",
            configuration(),
            Box::new(FailedClose(close_calls)),
            false,
        )
        .unwrap();
    sessions.operations.insert(
        ("alice".into(), "original-open".into()),
        OpenRecord {
            packet_boundaries: false,
            id: 1,
            encoder: identity.clone(),
            configuration: configuration(),
        },
    );
    assert!(sessions.close("alice", 1).is_err());
    assert_eq!(
        sessions.lookup("alice", "original-open"),
        Err("controlled_native_cleanup_failure")
    );
    assert_eq!(
        sessions.open(
            "alice",
            identity,
            configuration(),
            Some("original-open".into()),
            false,
        ),
        Err("controlled_native_cleanup_failure")
    );
    assert_eq!(
        sessions.lookup("mallory", "original-open"),
        Err("session_not_found")
    );
}

struct HeldWorker {
    entered: mpsc::Sender<()>,
    gate: Arc<(Mutex<bool>, Condvar)>,
    calls: Arc<AtomicUsize>,
}

impl WorkerBackend for HeldWorker {
    fn uses_background_encode(&self) -> bool {
        true
    }

    fn submit(&mut self, _: Vec<u8>, _: i64, _: bool) -> Result<(), &'static str> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.send(()).unwrap();
        let mut ready = self.gate.0.lock().unwrap();
        while !*ready {
            ready = self.gate.1.wait(ready).unwrap();
        }
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<Packet>, &'static str> {
        Ok(None)
    }

    fn drain(&mut self) -> Result<(), &'static str> {
        Ok(())
    }
}

struct ReleaseWorker(Arc<(Mutex<bool>, Condvar)>);

impl Drop for ReleaseWorker {
    fn drop(&mut self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }
}

unsafe extern "C" fn unused_runtime(_: *mut c_void, _: BorrowedBytes, _: ReplySink) -> Status {
    amaterasu_abi::STATUS_INTERNAL_ERROR
}

pub(super) fn module(sessions: Sessions) -> crate::EncodeModule {
    // SAFETY: this inert endpoint owns no state and these fixtures never send
    // runtime commands; the empty log queue is safe to flush on destruction.
    let runtime = unsafe {
        RuntimeClient::from_endpoint(HostApi {
            header: AbiHeader::of::<HostApi>(),
            context: std::ptr::null_mut(),
            invoke: unused_runtime,
        })
    };
    let (_, log_queue) = runtime.log_emitter();
    let shared = Arc::new(Mutex::new(sessions));
    crate::EncodeModule {
        runtime,
        _registration: Box::new(crate::native::Registration::new(shared.clone())),
        shared,
        log_queue,
        last_timing_log: Instant::now(),
        shutting_down: false,
        closing: crate::closing::Closing::default(),
    }
}

#[test]
fn module_shutdown_is_not_ready_while_an_encoder_worker_still_reads_a_frame() {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (entered, started) = mpsc::channel();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut worker = Worker::with_backend(HeldWorker {
        entered,
        gate: gate.clone(),
        calls: calls.clone(),
    });
    worker.submit(vec![1], 1, false).unwrap();
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    let release = ReleaseWorker(gate);
    assert_eq!(
        worker.submit(vec![2], 2, false).unwrap(),
        SubmitResult::Accepted
    );
    assert_eq!(
        worker.submit(vec![3], 3, false).unwrap(),
        SubmitResult::Accepted
    );
    let mut sessions = Sessions::new(true);
    sessions
        .insert("alice", configuration(), Box::new(worker), false)
        .unwrap();
    let mut module = module(sessions);
    let started = Instant::now();
    let ready = module.quiesce();
    let elapsed = started.elapsed();
    let state = module.shared.lock().unwrap().status("alice", 1);
    drop(release);
    assert_eq!(ready, Ok(false));
    assert_eq!(state.unwrap()["state"], "closing");
    assert!(elapsed < Duration::from_millis(250));
    let deadline = Instant::now() + Duration::from_secs(2);
    while !module.quiesce().unwrap() {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn broker_close_retains_the_exact_request_without_blocking_the_host_tick() {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (entered, started) = mpsc::channel();
    let mut worker = Worker::with_backend(HeldWorker {
        entered,
        gate: gate.clone(),
        calls: Arc::new(AtomicUsize::new(0)),
    });
    worker.submit(vec![1], 1, false).unwrap();
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    let mut sessions = Sessions::new(true);
    sessions
        .insert("alice", configuration(), Box::new(worker), false)
        .unwrap();
    let mut module = module(sessions);
    let broker = Broker::attach(&mut module);
    broker.request(912, "alice", json!({"action":"close","session_id":1}));
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        drop(ReleaseWorker(gate));
    });
    let started = Instant::now();
    module.tick().unwrap();
    let elapsed = started.elapsed();
    let premature_completion = broker.completed();
    releaser.join().unwrap();
    assert!(
        elapsed < Duration::from_millis(250),
        "tick blocked for {elapsed:?}"
    );
    assert!(premature_completion.is_empty());
    let deadline = Instant::now() + Duration::from_secs(2);
    while broker.completed().is_empty() {
        module.tick().unwrap();
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(
        broker.completed(),
        vec![(912, json!({"ok":true,"session_id":1,"state":"closed"}))]
    );
}

#[test]
fn shutdown_announcement_is_nonblocking_and_preserves_consumers_until_quiesce() {
    let mut module = module(Sessions::new(true));
    let shared = module.shared.clone();
    let held_native_call = shared.lock().unwrap();
    module.prepare_shutdown(20_000).unwrap();
    assert_eq!(module.quiesce(), Ok(false));
    assert_eq!(
        module.control(
            "alice",
            json!({"action":"probe","encoder":{"clsid":"fixture","friendly_name":"fixture"},
                "configuration":{"width":16,"height":16,"fps":60,"bitrate":1_000_000}})
        ),
        Err("encoder_shutting_down")
    );
    drop(held_native_call);
    assert_eq!(module.quiesce(), Ok(true));
}

#[test]
fn shutdown_announcement_does_not_begin_cleanup_or_reject_existing_consumer_frames() {
    let close_calls = Arc::new(AtomicUsize::new(0));
    let mut sessions = Sessions::new(true);
    sessions
        .insert(
            "alice",
            configuration(),
            Box::new(FailedClose(close_calls.clone())),
            false,
        )
        .unwrap();
    let mut module = module(sessions);
    module.prepare_shutdown(20_000).unwrap();
    let mut sessions = module.shared.lock().unwrap();
    assert_eq!(sessions.status("alice", 1).unwrap()["state"], "open");
    assert_eq!(close_calls.load(Ordering::SeqCst), 0);
    let pixels = vec![0; 16 * 16 * 3 / 2];
    let frame = CpuFrame {
        width: 16,
        height: 16,
        format: PIXEL_NV12,
        stride: 16,
        data: pixels.as_ptr(),
        len: pixels.len(),
        timestamp_us: 1,
        sequence: 1,
    };
    // SAFETY: initialized source bytes remain borrowed throughout the call.
    assert_eq!(unsafe { sessions.submit("alice", 1, &frame, 0) }, STATUS_OK);
}

#[test]
fn a_panicked_native_worker_cannot_be_reported_as_successfully_closed() {
    struct PanickedWorker;
    impl WorkerBackend for PanickedWorker {
        fn uses_background_encode(&self) -> bool {
            true
        }
        fn submit(&mut self, _: Vec<u8>, _: i64, _: bool) -> Result<(), &'static str> {
            panic!("controlled worker failure");
        }
        fn receive(&mut self) -> Result<Option<Packet>, &'static str> {
            Ok(None)
        }
        fn drain(&mut self) -> Result<(), &'static str> {
            Ok(())
        }
    }
    let mut worker = Worker::with_backend(PanickedWorker);
    worker.submit(vec![1], 1, false).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !worker.ready_to_close() {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    let mut sessions = Sessions::new(true);
    sessions
        .insert("alice", configuration(), Box::new(worker), false)
        .unwrap();
    assert_eq!(sessions.close("alice", 1), Err("encoder_shutdown_failed"));
    assert_eq!(sessions.close("alice", 1), Err("encoder_shutdown_failed"));
}

#[test]
fn failed_close_does_not_prevent_other_sessions_from_being_retired() {
    let close_calls = Arc::new(AtomicUsize::new(0));
    let mut sessions = Sessions::new(true);
    for owner in ["alice", "bob"] {
        sessions
            .insert(
                owner,
                configuration(),
                Box::new(FailedClose(close_calls.clone())),
                false,
            )
            .unwrap();
    }
    let mut module = module(sessions);
    assert_eq!(
        module.quiesce(),
        Err("controlled_native_cleanup_failure".into())
    );
    assert_eq!(close_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        module.quiesce(),
        Err("controlled_native_cleanup_failure".into())
    );
    assert_eq!(close_calls.load(Ordering::SeqCst), 2);
}
