//! Dedicated threads own native encoder state; no native pointer crosses threads.
use crate::{
    input::{GpuInput, InputSupport},
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
    SubmitGpu(GpuInput, i64, bool),
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
    native_stages: Option<NativeStageTimings>,
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
#[repr(C)]
pub struct NativeStageTimings {
    pub import_convert_us: u64,
    pub interop_copy_us: u64,
    pub map_input_us: u64,
    pub submit_picture_us: u64,
    pub lock_output_us: u64,
    pub stream_ordered: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct NativeStageTotals {
    pub samples: u64,
    pub import_convert_us: u64,
    pub interop_copy_us: u64,
    pub map_input_us: u64,
    pub submit_picture_us: u64,
    pub lock_output_us: u64,
    pub stream_ordered_samples: u64,
}

impl NativeStageTotals {
    fn record(&mut self, sample: NativeStageTimings) {
        self.samples = self.samples.saturating_add(1);
        self.import_convert_us = self
            .import_convert_us
            .saturating_add(sample.import_convert_us);
        self.interop_copy_us = self.interop_copy_us.saturating_add(sample.interop_copy_us);
        self.map_input_us = self.map_input_us.saturating_add(sample.map_input_us);
        self.submit_picture_us = self
            .submit_picture_us
            .saturating_add(sample.submit_picture_us);
        self.lock_output_us = self.lock_output_us.saturating_add(sample.lock_output_us);
        self.stream_ordered_samples = self
            .stream_ordered_samples
            .saturating_add(u64::from(sample.stream_ordered != 0));
    }
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
    pub native_stage_totals: Option<NativeStageTotals>,
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

/// Encoder state is created, used, and dropped inside the worker thread. The
/// backend intentionally has no `Send` bound because native handles may be
/// thread-affine; only the factory closure crosses the thread boundary.
pub(crate) trait WorkerBackend {
    fn uses_background_encode(&self) -> bool;
    fn submit(&mut self, data: Vec<u8>, timestamp: i64, force: bool) -> Result<(), &'static str>;
    fn receive(&mut self) -> Result<Option<Packet>, &'static str>;
    fn drain(&mut self) -> Result<(), &'static str>;
    fn native_stage_timings(&self) -> Option<NativeStageTimings> {
        None
    }
    /// Reported once, on the worker thread, right after the backend opens.
    fn input_support(&self) -> InputSupport {
        InputSupport::default()
    }
    /// Opt-in promise: submit_gpu finishes all source pixel reads on return,
    /// including failure. Merely importing or duplicating descriptors is not
    /// completion. Backends retaining input beyond return must leave false.
    fn supports_gpu_completion(&self) -> bool {
        false
    }
    fn submit_gpu(
        &mut self,
        _input: GpuInput,
        _timestamp: i64,
        _force: bool,
    ) -> Result<(), &'static str> {
        Err("gpu_input_unsupported")
    }
}

impl WorkerBackend for platform::Encoder {
    fn native_stage_timings(&self) -> Option<NativeStageTimings> {
        platform::Encoder::native_stage_timings(self)
    }
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

    fn input_support(&self) -> InputSupport {
        platform::Encoder::input_support(self)
    }

    fn supports_gpu_completion(&self) -> bool {
        // The only current DMA-BUF importer is native NVENC/CUDA. Its import
        // finishes source GL reads and synchronizes copies before returning.
        self.input_support().dmabuf.is_some()
    }
    fn submit_gpu(
        &mut self,
        input: GpuInput,
        timestamp: i64,
        force: bool,
    ) -> Result<(), &'static str> {
        platform::Encoder::submit_gpu(self, input, timestamp, force)
    }
}

pub struct Worker {
    sender: Option<SyncSender<Request>>,
    thread: Option<JoinHandle<()>>,
    cancel: Arc<CancellationGate>,
    background_encode: bool,
    input_support: InputSupport,
    gpu_completion: bool,
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

    pub fn open_with_log_emitter(
        identity: EncoderIdentity,
        config: Configuration,
        emitter: amaterasu_module_sdk::LogEmitter,
        notifier: amaterasu_module_sdk::WorkNotifier,
    ) -> Result<Self, &'static str> {
        Self::start_with_notifier(
            move || {
                platform::Encoder::open_with_log_emitter(&identity, &config, Some(emitter))
                    .map(|encoder| Box::new(encoder) as Box<dyn WorkerBackend>)
            },
            Some(notifier),
        )
    }

    fn start(
        open_backend: impl FnOnce() -> Result<Box<dyn WorkerBackend>, &'static str> + Send + 'static,
    ) -> Result<Self, &'static str> {
        Self::start_with_notifier(open_backend, None)
    }

    fn start_with_notifier(
        open_backend: impl FnOnce() -> Result<Box<dyn WorkerBackend>, &'static str> + Send + 'static,
        notifier: Option<amaterasu_module_sdk::WorkNotifier>,
    ) -> Result<Self, &'static str> {
        let (sender, receiver) = mpsc::sync_channel::<Request>(ASYNC_QUEUE_CAPACITY);
        let (ready, initialized) = mpsc::sync_channel(1);
        let cancel = Arc::new(CancellationGate::default());
        let worker_cancel = Arc::clone(&cancel);
        let thread = thread::Builder::new()
            .name("amaterasu-h264".into())
            .spawn(move || {
                amaterasu_module_sdk::threading::configure_worker_priority(
                    amaterasu_module_sdk::threading::ThreadPriority::High,
                );
                let mut encoder = match open_backend() {
                    Ok(encoder) => encoder,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                let background_encode = encoder.uses_background_encode();
                let gpu_completion = encoder.supports_gpu_completion();
                if ready
                    .send(Ok((
                        background_encode,
                        encoder.input_support(),
                        gpu_completion,
                    )))
                    .is_err()
                {
                    return;
                }
                let mut failed = false;
                while let Ok(request) = receiver.recv() {
                    let started_at = Instant::now();
                    #[cfg(unix)]
                    let is_dmabuf = matches!(
                        &request.operation,
                        Operation::SubmitGpu(GpuInput::DmaBuf(_), ..)
                    );
                    #[cfg(not(unix))]
                    let is_dmabuf = false;
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
                            Operation::SubmitGpu(input, timestamp, force) => encoder
                                .submit_gpu(input, timestamp, force)
                                .map(|()| Reply::Done),
                            Operation::Receive => encoder.receive().map(Reply::Packet),
                            Operation::Drain => encoder.drain().map(|()| Reply::Done),
                        }
                    };
                    let work_ready = result.is_err()
                        || matches!(&result, Ok(Reply::Packet(Some(_))))
                        || matches!(request.kind, AsyncKind::Submit | AsyncKind::Drain);
                    if result.is_err() {
                        failed = true;
                    }
                    let operation_us = duration_micros(started_at.elapsed());
                    let native_stages = if result.is_ok() && is_dmabuf {
                        encoder.native_stage_timings()
                    } else {
                        None
                    };
                    let delivered = request
                        .reply
                        .send(Completed {
                            result,
                            kind: request.kind,
                            queue_wait_us,
                            operation_us,
                            native_stages,
                        })
                        .is_ok();
                    if delivered
                        && work_ready
                        && let Some(notifier) = &notifier
                    {
                        notifier.notify();
                    }
                    if !delivered {
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
            Ok((background_encode, input_support, gpu_completion)) => Ok(Self {
                sender: Some(sender),
                thread: Some(thread),
                cancel,
                background_encode,
                input_support,
                gpu_completion,
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
                thread.join().map_err(|_| "encoder_shutdown_failed")?;
                Err(error)
            }
        }
    }

    // Test backends are captured by the Send factory closure; production
    // backends are instead constructed by that closure on the worker thread.
    #[cfg(test)]
    pub(crate) fn with_backend(backend: impl WorkerBackend + Send + 'static) -> Self {
        Self::start(move || Ok(Box::new(backend))).expect("controlled test backend opens")
    }

    fn request(&mut self, operation: Operation) -> Result<Reply, &'static str> {
        self.request_completion(operation, true)
    }

    /// A borrowed GPU lease cannot expire while the worker still reads it.
    /// This path therefore waits for native completion instead of timing out
    /// and returning ownership while vendor code continues in the background.
    fn request_completion(
        &mut self,
        operation: Operation,
        allow_timeout: bool,
    ) -> Result<Reply, &'static str> {
        if self.failed {
            return Err("encoder_failed");
        }
        let (reply, response) = mpsc::sync_channel(1);
        let kind = match &operation {
            Operation::Submit(..) | Operation::SubmitGpu(..) => AsyncKind::Submit,
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
        let completion = if allow_timeout {
            response
                .recv_timeout(Duration::from_secs(3))
                .map_err(|_| "encoder_timeout")
        } else {
            response.recv().map_err(|_| "encoder_worker_failed")
        };
        let completed = match completion {
            Ok(completed) => completed,
            Err(error) => {
                self.failed = true;
                return Err(error);
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
        if let Some(sample) = completed.native_stages {
            self.metrics
                .native_stage_totals
                .get_or_insert_with(Default::default)
                .record(sample);
        }
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

    /// Read only completions already observed by normal media operations.
    /// Logging must not consume a worker error or change operation ordering.
    pub fn observed_metrics(&self) -> Metrics {
        self.metrics
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

    pub fn input_support(&self) -> InputSupport {
        self.input_support.clone()
    }

    /// True when `submit_gpu` is guaranteed to be accepted rather than busy.
    /// DMA-BUF submission checks this before duplicating any descriptor.
    pub fn has_gpu_submit_capacity(&mut self) -> Result<bool, &'static str> {
        if !self.background_encode {
            // Synchronous submission waits for the worker instead of queuing.
            return Ok(true);
        }
        self.poll_background()?;
        // A second waiting GPU frame adds a full encode interval of latency.
        // Keep one submit in flight; receive/drain work may use the other slot.
        Ok(self.pending.len() < ASYNC_QUEUE_CAPACITY
            && !self
                .pending
                .iter()
                .any(|pending| matches!(pending.kind, AsyncKind::Submit)))
    }

    pub fn submit_gpu(
        &mut self,
        input: GpuInput,
        timestamp: i64,
        force: bool,
    ) -> Result<SubmitResult, &'static str> {
        if !self.background_encode {
            self.request(Operation::SubmitGpu(input, timestamp, force))?;
            return Ok(SubmitResult::Accepted);
        }
        self.poll_background()?;
        if self.enqueue(
            Operation::SubmitGpu(input, timestamp, force),
            AsyncKind::Submit,
        )? {
            Ok(SubmitResult::Accepted)
        } else {
            self.metrics.busy_submissions = self.metrics.busy_submissions.saturating_add(1);
            Ok(SubmitResult::Busy)
        }
    }

    pub fn supports_gpu_completion(&self) -> bool {
        self.gpu_completion
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
            // Keep one receive in flight across runtime ticks. Waiting for a
            // second caller tick to request it halves packet delivery cadence.
            let prefetch = !self.drain_enqueued || (self.drain_complete && packet.is_some());
            if prefetch && self.enqueue(Operation::Receive, AsyncKind::Receive)? {
                self.receive_pending = true;
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

impl Worker {
    /// Stop queue admission and skip queued operations after the current native
    /// call returns. The worker and its module code remain owned until joined.
    pub fn begin_shutdown(&mut self) {
        self.cancel.cancel();
        self.sender.take();
    }

    /// A finished thread can be joined without blocking the module's host tick.
    pub fn ready_to_close(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub fn shutdown(mut self) -> Result<(), &'static str> {
        self.begin_shutdown();
        if let Some(thread) = self.thread.take() {
            thread.join().map_err(|_| "encoder_shutdown_failed")?;
        }
        Ok(())
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
    #[test]
    fn native_stage_totals_preserve_independent_measurements_and_saturate() {
        let mut totals = super::NativeStageTotals::default();
        totals.record(super::NativeStageTimings {
            import_convert_us: 100,
            interop_copy_us: 200,
            map_input_us: 3,
            submit_picture_us: 4,
            lock_output_us: u64::MAX,
            stream_ordered: 1,
        });
        totals.record(super::NativeStageTimings {
            import_convert_us: 50,
            lock_output_us: 1,
            ..Default::default()
        });
        assert_eq!(totals.samples, 2);
        assert_eq!(totals.import_convert_us, 150);
        assert_eq!(totals.interop_copy_us, 200);
        assert_eq!(totals.lock_output_us, u64::MAX);
        assert_eq!(totals.stream_ordered_samples, 1);
    }
    use super::*;
    use std::sync::{Condvar, Mutex};

    struct PacketBackend(Arc<std::sync::atomic::AtomicUsize>);
    impl WorkerBackend for PacketBackend {
        fn uses_background_encode(&self) -> bool {
            true
        }
        fn submit(&mut self, _: Vec<u8>, _: i64, _: bool) -> Result<(), &'static str> {
            Ok(())
        }
        fn drain(&mut self) -> Result<(), &'static str> {
            Ok(())
        }
        fn receive(&mut self) -> Result<Option<Packet>, &'static str> {
            let call = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Some(Packet {
                ready_at: None,
                bytes: vec![call as u8],
                timestamp_100ns: call as i64,
                keyframe: call == 0,
            }))
        }
    }

    #[test]
    fn readiness_wakes_for_packets_and_submits_but_not_empty_receive_polls() {
        use amaterasu_module_sdk::{RuntimeClient, abi::*};
        use std::sync::atomic::{AtomicUsize, Ordering};
        unsafe extern "C" fn notify(
            context: *mut std::ffi::c_void,
            input: BorrowedBytes,
            reply: ReplySink,
        ) -> Status {
            // SAFETY: fixture and input are retained through this worker callback.
            let count = unsafe { &*context.cast::<AtomicUsize>() };
            let command: amaterasu_module_sdk::Command =
                serde_json::from_slice(unsafe { input.read().unwrap() }).unwrap();
            assert!(matches!(
                command,
                amaterasu_module_sdk::Command::NotifyWorkReady {}
            ));
            count.fetch_add(1, Ordering::SeqCst);
            // SAFETY: the SDK lends this sink until synchronous invoke returns.
            unsafe { reply.send(br#"{"kind":"ok"}"#) };
            STATUS_OK
        }
        struct Backend {
            calls: Arc<AtomicUsize>,
        }
        impl WorkerBackend for Backend {
            fn uses_background_encode(&self) -> bool {
                true
            }
            fn submit(&mut self, _: Vec<u8>, _: i64, _: bool) -> Result<(), &'static str> {
                Ok(())
            }
            fn drain(&mut self) -> Result<(), &'static str> {
                Ok(())
            }
            fn receive(&mut self) -> Result<Option<Packet>, &'static str> {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                Ok((call == 1).then(|| Packet {
                    ready_at: None,
                    bytes: vec![1],
                    timestamp_100ns: 0,
                    keyframe: true,
                }))
            }
        }
        let notifications = AtomicUsize::new(0);
        let endpoint = HostApi {
            header: AbiHeader::of::<HostApi>(),
            context: (&notifications as *const AtomicUsize).cast_mut().cast(),
            invoke: notify,
        };
        // SAFETY: notifications and endpoint live until worker is joined below.
        let runtime = unsafe { RuntimeClient::from_endpoint(endpoint) };
        let calls = Arc::new(AtomicUsize::new(0));
        let backend_calls = calls.clone();
        let mut worker = Worker::start_with_notifier(
            move || {
                Ok(Box::new(Backend {
                    calls: backend_calls,
                }))
            },
            Some(runtime.work_notifier()),
        )
        .unwrap();
        assert!(matches!(worker.receive().unwrap(), ReceiveResult::Pending));
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            worker.poll_background().unwrap();
            if worker.receive_result.is_some() {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        assert_eq!(
            notifications.load(Ordering::SeqCst),
            0,
            "empty receive created a polling wake loop"
        );
        assert!(matches!(
            worker.receive().unwrap(),
            ReceiveResult::Ready(None)
        ));
        while notifications.load(Ordering::SeqCst) < 1 {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        loop {
            match worker.receive().unwrap() {
                ReceiveResult::Ready(Some(packet)) => {
                    assert_eq!(packet.bytes, [1]);
                    break;
                }
                _ => {
                    assert!(Instant::now() < deadline);
                    thread::yield_now();
                }
            }
        }
        assert_eq!(
            worker.submit(vec![0], 1, false).unwrap(),
            SubmitResult::Accepted
        );
        while notifications.load(Ordering::SeqCst) < 2 {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        drop(worker);
        assert_eq!(notifications.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn diagnostic_snapshot_does_not_consume_pending_worker_results() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let backend_calls = calls.clone();
        let mut worker = Worker::start(move || Ok(Box::new(PacketBackend(backend_calls)))).unwrap();
        assert!(matches!(worker.receive().unwrap(), ReceiveResult::Pending));
        let deadline = Instant::now() + Duration::from_secs(1);
        while calls.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        assert_eq!(worker.observed_metrics().completed_operations, 0);
        assert_eq!(worker.pending.len(), 1);
        assert!(worker.receive_result.is_none());
        loop {
            match worker.receive().unwrap() {
                ReceiveResult::Ready(Some(packet)) => {
                    assert_eq!(packet.bytes, [0]);
                    break;
                }
                ReceiveResult::Pending => {}
                _ => panic!("pending packet was lost"),
            }
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        assert_eq!(worker.observed_metrics().receive_operations, 1);
    }

    #[test]
    fn consuming_a_packet_prefetches_only_one_next_receive_without_an_extra_tick() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let backend_calls = calls.clone();
        let mut worker = Worker::start(move || Ok(Box::new(PacketBackend(backend_calls)))).unwrap();
        assert!(matches!(worker.receive().unwrap(), ReceiveResult::Pending));
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            worker.metrics().unwrap();
            if worker.receive_result.is_some() {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        let ReceiveResult::Ready(Some(first)) = worker.receive().unwrap() else {
            panic!("first packet not ready")
        };
        assert_eq!(first.bytes, [0]);
        let deadline = Instant::now() + Duration::from_secs(1);
        while calls.load(Ordering::SeqCst) < 2 {
            assert!(
                Instant::now() < deadline,
                "next receive waited for another caller tick"
            );
            thread::yield_now();
        }
        worker.metrics().unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "receive prefetch grew without consumption"
        );
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match worker.receive().unwrap() {
                ReceiveResult::Ready(Some(second)) => {
                    assert_eq!(second.bytes, [1]);
                    break;
                }
                ReceiveResult::Pending => {}
                _ => panic!("second packet was lost"),
            }
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
    }

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
