use super::{probe_support::*, *};
use crate::{
    factory::Factory,
    model::{Configuration, EncoderDescription, EncoderIdentity, Packet},
    sessions::Backend,
    worker::{ReceiveResult, SubmitResult},
};
use std::sync::{
    Condvar,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

struct Gate(Arc<(Mutex<bool>, Condvar)>);
impl Drop for Gate {
    fn drop(&mut self) {
        let (lock, wake) = &*self.0;
        *lock.lock().unwrap() = true;
        wake.notify_all();
    }
}
struct SlowFactory {
    gate: Arc<(Mutex<bool>, Condvar)>,
    entered: mpsc::Sender<()>,
    closed: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
}
impl SlowFactory {
    fn wait(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.send(()).unwrap();
        let (lock, wake) = &*self.gate;
        let (released, timed) = wake
            .wait_timeout_while(lock.lock().unwrap(), Duration::from_secs(3), |released| {
                !*released
            })
            .unwrap();
        assert!(
            *released && !timed.timed_out(),
            "test preparation was never released"
        );
    }
}
impl Factory for SlowFactory {
    fn enumerate(&self) -> Result<Vec<EncoderDescription>, &'static str> {
        self.wait();
        Ok(vec![])
    }
    fn open(&self, _: EncoderIdentity, _: Configuration) -> Result<Box<dyn Backend>, &'static str> {
        self.wait();
        Ok(Box::new(ProbeBackend {
            packet: None,
            closed: self.closed.clone(),
        }))
    }
}
struct ProbeBackend {
    packet: Option<Packet>,
    closed: Arc<AtomicUsize>,
}
impl Backend for ProbeBackend {
    fn close(self: Box<Self>) -> Result<(), &'static str> {
        self.closed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn submit(
        &mut self,
        _: Vec<u8>,
        timestamp: i64,
        _: bool,
    ) -> Result<SubmitResult, &'static str> {
        self.packet = Some(Packet {
            ready_at: None,
            bytes: vec![
                0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 1, 0, 0, 0, 1, 0x65, 1,
            ],
            timestamp_100ns: timestamp,
            keyframe: true,
        });
        Ok(SubmitResult::Accepted)
    }
    fn receive(&mut self) -> Result<ReceiveResult, &'static str> {
        Ok(ReceiveResult::Ready(self.packet.take()))
    }
    fn drain(&mut self) -> Result<bool, &'static str> {
        Ok(true)
    }
}
fn install(
    module: &EncodeModule,
) -> (Gate, mpsc::Receiver<()>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (entered, waiting) = mpsc::channel();
    let closed = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    *module.shared.lock().unwrap() = crate::sessions::Sessions::with_factory(
        true,
        Arc::new(SlowFactory {
            gate: gate.clone(),
            entered,
            closed: closed.clone(),
            calls: calls.clone(),
        }),
    );
    (Gate(gate), waiting, closed, calls)
}
fn until_reply(host: &Host, module: &mut EncodeModule, id: u64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        module.tick().unwrap();
        if let Some(reply) = host.take_reply(id) {
            return reply;
        }
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
}
#[test]
fn half_second_discovery_probe_and_open_do_not_hold_the_host_tick_or_native_lock() {
    for action in ["enumerate", "probe", "open"] {
        let (host, mut module) = Host::start();
        let (gate, entered, closed, _) = install(&module);
        let payload = match action {
            "enumerate" => json!({"action":"enumerate"}),
            "probe" => probe_request(&hardware_identity(), &configuration()),
            _ => open_request(&hardware_identity(), &configuration()),
        };
        host.queue(42, payload);
        let started = Instant::now();
        module.tick().unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "{action} held the host tick"
        );
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            module.shared.try_lock().is_ok(),
            "{action} held the native session lock"
        );
        // This unrelated broker request must finish while preparation is still
        // blocked, reproducing the shared-thread starvation in the live trace.
        host.queue(43, json!({"action":"lookup","operation_id":"unrelated"}));
        let started = Instant::now();
        let reply = until_reply(&host, &mut module, 43);
        assert_eq!(reply["code"], "session_not_found");
        assert!(started.elapsed() < Duration::from_millis(250));
        assert!(host.take_reply(42).is_none());
        std::thread::sleep(Duration::from_millis(500));
        let started = Instant::now();
        module.tick().unwrap();
        assert!(started.elapsed() < Duration::from_millis(250));
        drop(gate);
        assert_eq!(until_reply(&host, &mut module, 42)["ok"], true);
        drop(module);
        assert_eq!(
            closed.load(Ordering::SeqCst),
            usize::from(action != "enumerate")
        );
    }
}
#[test]
fn cancelled_open_is_retired_before_another_open_can_start_and_never_allocates_a_session() {
    let (host, mut module) = Host::start();
    let (gate, entered, closed, calls) = install(&module);
    host.queue(42, open_request(&hardware_identity(), &configuration()));
    module.tick().unwrap();
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    host.cancel(42);
    host.queue(43, open_request(&hardware_identity(), &configuration()));
    module.tick().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(module.quiesce(), Ok(false));
    drop(gate);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !module.quiesce().unwrap() {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(closed.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(module.shared.lock().unwrap().latest_id(), 0);
    assert!(host.take_reply(42).is_none());
    assert_eq!(
        host.take_reply(43).unwrap()["code"],
        "encoder_shutting_down"
    );
}
#[test]
fn preparation_admission_is_bounded_and_cancelled_queued_requests_do_not_run() {
    let (host, mut module) = Host::start();
    let (gate, entered, _, calls) = install(&module);
    host.queue(42, json!({"action":"enumerate"}));
    module.tick().unwrap();
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    for id in 43..=58 {
        host.queue(id, json!({"action":"enumerate"}));
        module.tick().unwrap();
    }
    assert_eq!(
        host.take_reply(58).unwrap()["code"],
        "encoder_preparation_limit"
    );
    for id in 43..58 {
        host.cancel(id);
    }
    drop(gate);
    assert_eq!(until_reply(&host, &mut module, 42)["ok"], true);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !module.preparation.queued_empty_for_test() {
        module.tick().unwrap();
        assert!(Instant::now() < deadline);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn cancelled_preparation_retires_the_backend_before_publishing_a_successor() {
    let (host, mut module) = Host::start();
    let (gate, entered, closed, calls) = install(&module);
    host.queue(42, open_request(&hardware_identity(), &configuration()));
    module.tick().unwrap();
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    host.cancel(42);
    host.queue(43, open_request(&hardware_identity(), &configuration()));
    module.tick().unwrap();
    drop(gate);
    let reply = until_reply(&host, &mut module, 43);
    assert_eq!(reply["session_id"], 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(closed.load(Ordering::SeqCst), 1);
    assert!(host.take_reply(42).is_none());
    drop(module);
    assert_eq!(closed.load(Ordering::SeqCst), 2);
}

#[test]
fn orphan_retirement_does_not_hold_the_host_tick_and_quiescence_waits_for_it() {
    struct RetiringBackend {
        entered: mpsc::Sender<()>,
        gate: Arc<(Mutex<bool>, Condvar)>,
    }
    impl Backend for RetiringBackend {
        fn submit(&mut self, _: Vec<u8>, _: i64, _: bool) -> Result<SubmitResult, &'static str> {
            Ok(SubmitResult::Accepted)
        }
        fn receive(&mut self) -> Result<ReceiveResult, &'static str> {
            Ok(ReceiveResult::Ready(None))
        }
        fn drain(&mut self) -> Result<bool, &'static str> {
            Ok(true)
        }
        fn close(self: Box<Self>) -> Result<(), &'static str> {
            self.entered.send(()).unwrap();
            let (lock, wake) = &*self.gate;
            let (released, _) = wake
                .wait_timeout_while(lock.lock().unwrap(), Duration::from_secs(3), |released| {
                    !*released
                })
                .unwrap();
            assert!(*released);
            Ok(())
        }
    }
    struct Once(Mutex<Option<Box<dyn Backend>>>);
    impl Factory for Once {
        fn enumerate(&self) -> Result<Vec<EncoderDescription>, &'static str> {
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
    let (host, mut module) = Host::start();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let release = Gate(gate.clone());
    let (started, entered) = mpsc::channel();
    *module.shared.lock().unwrap() = crate::sessions::Sessions::with_factory(
        true,
        Arc::new(Once(Mutex::new(Some(Box::new(RetiringBackend {
            entered: started,
            gate,
        }))))),
    );
    host.queue(42, open_request(&hardware_identity(), &configuration()));
    module.tick().unwrap();
    host.cancel(42);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let started = Instant::now();
        module.tick().unwrap();
        assert!(started.elapsed() < Duration::from_millis(250));
        if entered.try_recv().is_ok() {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    host.queue(43, json!({"action":"lookup","operation_id":"unrelated"}));
    assert_eq!(
        until_reply(&host, &mut module, 43)["code"],
        "session_not_found"
    );
    assert_eq!(module.quiesce(), Ok(false));
    drop(release);
    while !module.quiesce().unwrap() {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(module.shared.lock().unwrap().latest_id(), 0);
}

#[test]
fn lookup_never_reports_absence_while_a_cancelled_open_still_owns_native_preparation() {
    let (host, mut module) = Host::start();
    let (gate, entered, closed, _) = install(&module);
    host.queue(42, open_request(&hardware_identity(), &configuration()));
    module.tick().unwrap();
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    host.cancel(42);
    host.queue(43, json!({"action":"lookup","operation_id":"winner-open"}));
    module.tick().unwrap();
    for _ in 0..20 {
        module.tick().unwrap();
    }
    assert!(host.take_reply(43).is_none());
    assert_eq!(closed.load(Ordering::SeqCst), 0);
    drop(gate);
    assert_eq!(
        until_reply(&host, &mut module, 43)["code"],
        "session_not_found"
    );
    assert_eq!(closed.load(Ordering::SeqCst), 1);
    assert_eq!(module.shared.lock().unwrap().latest_id(), 0);
}

#[test]
fn queued_duplicate_open_and_lookup_observe_one_published_session() {
    let (host, mut module) = Host::start();
    let (gate, entered, closed, calls) = install(&module);
    host.queue(42, open_request(&hardware_identity(), &configuration()));
    module.tick().unwrap();
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    host.queue(43, open_request(&hardware_identity(), &configuration()));
    module.tick().unwrap();
    host.queue(44, json!({"action":"lookup","operation_id":"winner-open"}));
    module.tick().unwrap();
    assert!(host.take_reply(44).is_none());
    drop(gate);
    assert_eq!(until_reply(&host, &mut module, 42)["session_id"], 1);
    assert_eq!(until_reply(&host, &mut module, 43)["session_id"], 1);
    assert_eq!(until_reply(&host, &mut module, 44)["session_id"], 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(module);
    assert_eq!(closed.load(Ordering::SeqCst), 1);
}
