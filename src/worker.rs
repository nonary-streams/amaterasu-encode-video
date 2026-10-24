//! Dedicated threads own native encoder state; no native pointer crosses threads.
use crate::{
    model::{Configuration, EncoderDescription, EncoderIdentity, Packet},
    platform,
};
use serde::Serialize;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const ASYNC_QUEUE_CAPACITY: usize = 2;
const ASYNC_MAX_OUTSTANDING: usize = ASYNC_QUEUE_CAPACITY + 1;

#[derive(Default)]
struct CancellationGate {
    cancelled: Mutex<bool>,
}

impl CancellationGate {
    fn may_start_operation(&self) -> bool {
        !*self
            .cancelled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn cancel(&self) {
        *self
            .cancelled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    }

    fn is_cancelled(&self) -> bool {
        *self
            .cancelled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

enum Operation {
    Submit(Vec<u8>, i64, bool),
    Receive,
    Drain,
}

#[derive(Clone, Copy)]
enum AsyncKind {
    Submit,
    Receive,
    Drain,
}

enum Reply {
    Done,
    Packet(Option<Packet>),
}

struct Completed {
    result: Result<Reply, &'static str>,
    kind: AsyncKind,
    queue_wait_us: u64,
    operation_us: u64,
}

struct Request {
    operation: Operation,
    kind: AsyncKind,
    queued_at: Instant,
    reply: SyncSender<Completed>,
}

struct Pending {
    kind: AsyncKind,
    response: Receiver<Completed>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Metrics {
    pub queue_wait_us_total: u64,
    pub operation_us_total: u64,
    pub operation_us_max: u64,
    pub completed_operations: u64,
    pub submit_operations: u64,
    pub submit_total_us: u64,
    pub submit_max_us: u64,
    pub receive_operations: u64,
    pub receive_total_us: u64,
    pub receive_max_us: u64,
    pub drain_operations: u64,
    pub drain_total_us: u64,
    pub drain_max_us: u64,
    pub busy_submissions: u64,
    pub pending_operations: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitResult {
    Accepted,
    Busy,
}

pub enum ReceiveResult {
    Ready(Option<Packet>),
    Pending,
}

pub(crate) trait WorkerBackend: Send {
    fn uses_background_encode(&self) -> bool;
    fn submit(&mut self, data: Vec<u8>, timestamp: i64, force: bool) -> Result<(), &'static str>;
    fn receive(&mut self) -> Result<Option<Packet>, &'static str>;
    fn drain(&mut self) -> Result<(), &'static str>;
}

impl WorkerBackend for platform::Encoder {
    fn uses_background_encode(&self) -> bool {
        platform::Encoder::uses_background_encode(self)
    }

    fn submit(&mut self, data: Vec<u8>, timestamp: i64, force: bool) -> Result<(), &'static str> {
        platform::Encoder::submit(self, data, timestamp, force)
    }

    fn receive(&mut self) -> Result<Option<Packet>, &'static str> {
        platform::Encoder::receive(self)
    }

    fn drain(&mut self) -> Result<(), &'static str> {
        platform::Encoder::drain(self)
    }
}

pub struct Worker {
    sender: Option<SyncSender<Request>>,
    thread: Option<JoinHandle<()>>,
    cancel: Arc<CancellationGate>,
    background_encode: bool,
    pending: VecDeque<Pending>,
    receive_result: Option<Option<Packet>>,
    receive_pending: bool,
    drain_enqueued: bool,
    drain_complete: bool,
    failed: bool,
    metrics: Metrics,
}

impl Worker {
    pub fn open(identity: EncoderIdentity, config: Configuration) -> Result<Self, &'static str> {
        Self::start(move || {
            platform::Encoder::open(&identity, &config)
                .map(|encoder| Box::new(encoder) as Box<dyn WorkerBackend>)
        })
    }

    fn start(
        open_backend: impl FnOnce() -> Result<Box<dyn WorkerBackend>, &'static str> + Send + 'static,
    ) -> Result<Self, &'static str> {
        let (sender, receiver) = mpsc::sync_channel::<Request>(ASYNC_QUEUE_CAPACITY);
        let (ready, initialized) = mpsc::sync_channel(1);
        let cancel = Arc::new(CancellationGate::default());
        let worker_cancel = Arc::clone(&cancel);
        let thread = thread::Builder::new()
            .name("amaterasu-h264".into())
            .spawn(move || {
                let mut encoder = match open_backend() {
                    Ok(encoder) => encoder,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                let background_encode = encoder.uses_background_encode();
                if ready.send(Ok(background_encode)).is_err() {
                    return;
                }
                let mut failed = false;
                while let Ok(request) = receiver.recv() {
                    let started_at = Instant::now();
                    let queue_wait_us =
                        duration_micros(started_at.saturating_duration_since(request.queued_at));
                    let result = if failed {
                        Err("encoder_failed")
                    } else if !worker_cancel.may_start_operation() {
                        Err("encoder_cancelled")
                    } else {
                        match request.operation {
                            Operation::Submit(data, timestamp, force) => {
                                encoder.submit(data, timestamp, force).map(|()| Reply::Done)
                            }
                            Operation::Receive => encoder.receive().map(Reply::Packet),
                            Operation::Drain => encoder.drain().map(|()| Reply::Done),
                        }
                    };
                    if result.is_err() {
                        failed = true;
                    }
                    let operation_us = duration_micros(started_at.elapsed());
                    if request
                        .reply
                        .send(Completed {
                            result,
                            kind: request.kind,
                            queue_wait_us,
                            operation_us,
                        })
                        .is_err()
                    {
                        // An abandoned asynchronous reply is expected during close.
                        if worker_cancel.is_cancelled() {
                            continue;
                        }
                    }
                }
            })
            .map_err(|_| "encoder_worker_failed")?;
        // Initialization may enter vendor driver code. Never detach module-owned
        // code on timeout: wait for native return before permitting unload.
        match initialized
            .recv()
            .map_err(|_| "encoder_worker_failed")
            .and_then(|value| value)
        {
            Ok(background_encode) => Ok(Self {
                sender: Some(sender),
                thread: Some(thread),
                cancel,
                background_encode,
                pending: VecDeque::new(),
                receive_result: None,
                receive_pending: false,
                drain_enqueued: false,
                drain_complete: false,
                failed: false,
                metrics: Metrics::default(),
            }),
            Err(error) => {
                cancel.cancel();
                drop(sender);
                let _ = thread.join();
                Err(error)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn with_backend(backend: impl WorkerBackend + 'static) -> Self {
        Self::start(move || Ok(Box::new(backend))).expect("controlled test backend opens")
    }

    fn request(&mut self, operation: Operation) -> Result<Reply, &'static str> {
        if self.failed {
            return Err("encoder_failed");
        }
        let (reply, response) = mpsc::sync_channel(1);
        let kind = match &operation {
            Operation::Submit(..) => AsyncKind::Submit,
            Operation::Receive => AsyncKind::Receive,
            Operation::Drain => AsyncKind::Drain,
        };
        self.sender
            .as_ref()
            .ok_or("encoder_worker_failed")?
            .try_send(Request {
                operation,
                kind,
                queued_at: Instant::now(),
                reply,
            })
            .map_err(|_| "encoder_worker_failed")?;
        let completed = match response.recv_timeout(Duration::from_secs(3)) {
            Ok(completed) => completed,
            Err(_) => {
                self.failed = true;
                return Err("encoder_timeout");
            }
        };
        self.record_completion(&completed);
        match completed.result {
            Ok(reply) => Ok(reply),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    fn enqueue(&mut self, operation: Operation, kind: AsyncKind) -> Result<bool, &'static str> {
        if self.failed {
            return Err("encoder_failed");
        }
        if !can_enqueue(self.pending.len()) {
            return Ok(false);
        }
        let (reply, response) = mpsc::sync_channel(1);
        let request = Request {
            operation,
            kind,
            queued_at: Instant::now(),
            reply,
        };
        match self
            .sender
            .as_ref()
            .ok_or("encoder_worker_failed")?
            .try_send(request)
        {
            Ok(()) => {
                self.pending.push_back(Pending { kind, response });
                self.metrics.pending_operations = self.pending.len() as u64;
                Ok(true)
            }
            Err(TrySendError::Full(_)) => Ok(false),
            Err(TrySendError::Disconnected(_)) => {
                self.failed = true;
                Err("encoder_worker_failed")
            }
        }
    }

    fn poll_background(&mut self) -> Result<(), &'static str> {
        while let Some(pending) = self.pending.front() {
            let completed = match pending.response.try_recv() {
                Ok(completed) => completed,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.failed = true;
                    return Err("encoder_worker_failed");
                }
            };
            let pending = self.pending.pop_front().expect("front item exists");
            self.metrics.pending_operations = self.pending.len() as u64;
            self.record_completion(&completed);
            match completed.result {
                Ok(Reply::Done) => {
                    if matches!(pending.kind, AsyncKind::Drain) {
                        self.drain_complete = true;
                    }
                }
                Ok(Reply::Packet(packet)) => {
                    self.receive_pending = false;
                    self.receive_result = Some(packet);
                }
                Err(error) => {
                    self.failed = true;
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    fn record_completion(&mut self, completed: &Completed) {
        self.metrics.queue_wait_us_total = self
            .metrics
            .queue_wait_us_total
            .saturating_add(completed.queue_wait_us);
        self.metrics.operation_us_total = self
            .metrics
            .operation_us_total
            .saturating_add(completed.operation_us);
        self.metrics.operation_us_max = self.metrics.operation_us_max.max(completed.operation_us);
        self.metrics.completed_operations = self.metrics.completed_operations.saturating_add(1);
        let (count, total, maximum) = match completed.kind {
            AsyncKind::Submit => (
                &mut self.metrics.submit_operations,
                &mut self.metrics.submit_total_us,
                &mut self.metrics.submit_max_us,
            ),
            AsyncKind::Receive => (
                &mut self.metrics.receive_operations,
                &mut self.metrics.receive_total_us,
                &mut self.metrics.receive_max_us,
            ),
            AsyncKind::Drain => (
                &mut self.metrics.drain_operations,
                &mut self.metrics.drain_total_us,
                &mut self.metrics.drain_max_us,
            ),
        };
        *count = count.saturating_add(1);
        *total = total.saturating_add(completed.operation_us);
        *maximum = (*maximum).max(completed.operation_us);
    }

    pub fn metrics(&mut self) -> Result<Metrics, &'static str> {
        self.poll_background()?;
        Ok(self.metrics)
    }

    pub fn submit(
        &mut self,
        data: Vec<u8>,
        timestamp: i64,
        force: bool,
    ) -> Result<SubmitResult, &'static str> {
        if !self.background_encode {
            self.request(Operation::Submit(data, timestamp, force))?;
            return Ok(SubmitResult::Accepted);
        }
        self.poll_background()?;
        if self.enqueue(Operation::Submit(data, timestamp, force), AsyncKind::Submit)? {
            Ok(SubmitResult::Accepted)
        } else {
            self.metrics.busy_submissions = self.metrics.busy_submissions.saturating_add(1);
            Ok(SubmitResult::Busy)
        }
    }

    pub fn receive(&mut self) -> Result<ReceiveResult, &'static str> {
        if !self.background_encode {
            return match self.request(Operation::Receive)? {
                Reply::Packet(packet) => Ok(ReceiveResult::Ready(packet)),
                _ => Err("encoder_worker_failed"),
            };
        }
        self.poll_background()?;
        if let Some(packet) = self.receive_result.take() {
            if packet.is_none() && self.drain_enqueued && !self.drain_complete {
                return Ok(ReceiveResult::Pending);
            }
            return Ok(ReceiveResult::Ready(packet));
        }
        if self.receive_pending || (self.drain_enqueued && !self.drain_complete) {
            return Ok(ReceiveResult::Pending);
        }
        if self.enqueue(Operation::Receive, AsyncKind::Receive)? {
            self.receive_pending = true;
        }
        Ok(ReceiveResult::Pending)
    }

    pub fn drain(&mut self) -> Result<bool, &'static str> {
        if !self.background_encode {
            self.request(Operation::Drain)?;
            return Ok(true);
        }
        self.poll_background()?;
        if self.drain_enqueued {
            return Ok(true);
        }
        if self.enqueue(Operation::Drain, AsyncKind::Drain)? {
            self.drain_enqueued = true;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Native calls are not forcibly interruptible. Mark cancellation so the
        // worker skips queued frames immediately after its current call returns.
        self.cancel.cancel();
        self.sender.take();
        // Keep module-owned native code loaded until an in-flight call returns.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn duration_micros(duration: Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

fn can_enqueue(outstanding: usize) -> bool {
    outstanding < ASYNC_MAX_OUTSTANDING
}

pub fn enumerate() -> Result<Vec<EncoderDescription>, &'static str> {
    thread::spawn(platform::enumerate)
        .join()
        .map_err(|_| "encoder_worker_failed")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Condvar, Mutex};

    #[derive(Default)]
    struct DropGateState {
        entered: bool,
        release: bool,
        submit_calls: usize,
    }

    #[derive(Default)]
    struct DropGate {
        state: Mutex<DropGateState>,
        changed: Condvar,
    }

    struct SlowBackend(Arc<DropGate>);

    struct ReleaseGateOnDrop(Arc<DropGate>);

    impl Drop for ReleaseGateOnDrop {
        fn drop(&mut self) {
            self.0.state.lock().unwrap().release = true;
            self.0.changed.notify_all();
        }
    }

    impl WorkerBackend for SlowBackend {
        fn uses_background_encode(&self) -> bool {
            true
        }

        fn submit(&mut self, _: Vec<u8>, _: i64, _: bool) -> Result<(), &'static str> {
            let mut state = self.0.state.lock().unwrap();
            state.submit_calls += 1;
            if state.submit_calls == 1 {
                state.entered = true;
                self.0.changed.notify_all();
                while !state.release {
                    state = self.0.changed.wait(state).unwrap();
                }
            }
            self.0.changed.notify_all();
            Ok(())
        }

        fn receive(&mut self) -> Result<Option<Packet>, &'static str> {
            Ok(None)
        }

        fn drain(&mut self) -> Result<(), &'static str> {
            Ok(())
        }
    }

    fn wait_for(gate: &DropGate, predicate: impl Fn(&DropGateState) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut state = gate.state.lock().unwrap();
        while !predicate(&state) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, timed) = gate.changed.wait_timeout(state, remaining).unwrap();
            state = next;
            if timed.timed_out() && !predicate(&state) {
                return false;
            }
        }
        true
    }

    #[test]
    fn background_queue_is_bounded_to_running_plus_two_queued_operations() {
        assert!(can_enqueue(0));
        assert!(can_enqueue(1));
        assert!(can_enqueue(2));
        assert!(!can_enqueue(ASYNC_MAX_OUTSTANDING));
        assert_eq!(ASYNC_MAX_OUTSTANDING, 3);
    }

    #[test]
    fn duration_metrics_use_bounded_microsecond_units() {
        assert_eq!(duration_micros(Duration::from_nanos(999)), 0);
        assert_eq!(duration_micros(Duration::from_micros(1234)), 1234);
    }

    #[test]
    fn close_cancels_queued_work_but_never_claims_to_interrupt_running_native_calls() {
        let cancellation = CancellationGate::default();
        assert!(cancellation.may_start_operation());
        cancellation.cancel();
        assert!(cancellation.is_cancelled());
        assert!(!cancellation.may_start_operation());
    }

    #[test]
    fn bounded_queue_drop_skips_queued_calls_and_joins_the_in_flight_call() {
        let gate = Arc::new(DropGate::default());
        let mut worker = Worker::with_backend(SlowBackend(gate.clone()));
        let _release_gate_on_exit = ReleaseGateOnDrop(gate.clone());
        assert_eq!(
            worker.submit(vec![1], 10, false).unwrap(),
            SubmitResult::Accepted
        );
        let entered = wait_for(&gate, |state| state.entered);
        if !entered {
            gate.state.lock().unwrap().release = true;
            gate.changed.notify_all();
        }
        assert!(entered);
        assert_eq!(
            worker.submit(vec![2], 20, false).unwrap(),
            SubmitResult::Accepted
        );
        assert_eq!(
            worker.submit(vec![3], 30, false).unwrap(),
            SubmitResult::Accepted
        );
        assert_eq!(
            worker.submit(vec![4], 40, false).unwrap(),
            SubmitResult::Busy
        );

        let cancelled = Arc::clone(&worker.cancel);
        let (finished, finished_rx) = mpsc::channel();
        let closer = thread::spawn(move || {
            drop(worker);
            let _ = finished.send(());
        });
        let cancel_deadline = Instant::now() + Duration::from_secs(2);
        while !cancelled.is_cancelled() && Instant::now() < cancel_deadline {
            thread::yield_now();
        }
        let cancellation_seen = cancelled.is_cancelled();
        let drop_waited_for_active_call =
            matches!(finished_rx.try_recv(), Err(mpsc::TryRecvError::Empty));
        {
            gate.state.lock().unwrap().release = true;
            gate.changed.notify_all();
        }
        finished_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker joins once its active call returns");
        closer.join().unwrap();
        assert!(cancellation_seen);
        assert!(drop_waited_for_active_call);
        assert_eq!(gate.state.lock().unwrap().submit_calls, 1);
    }
}
