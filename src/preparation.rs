//! Native discovery, probing and opening never run on the shared module thread.
//! One worker owns slow preparation and orphan retirement. Broker requests and
//! session admission remain on the module thread, with bounded pending work.
use crate::{
    closing::Closing,
    factory::Factory,
    model::{Configuration, EncoderIdentity, Operation},
    native::Shared,
    platform, probe,
    sessions::Backend,
};
use amaterasu_module_sdk::{
    Command, LogLevel, ModuleResult, Response, RuntimeClient, WorkNotifier,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const MAX_PENDING: usize = 16;
struct Waiter {
    id: u64,
    owner: String,
    open_operation: Option<String>,
}
struct Request {
    waiter: Waiter,
    operation: Operation,
}
struct Open {
    encoder: EncoderIdentity,
    configuration: Configuration,
    operation: Option<String>,
    packet_boundaries: bool,
}
enum Admission {
    Work(Work),
    Reply(Value),
}

enum Work {
    Enumerate {
        factory: Arc<dyn Factory>,
        enabled: bool,
    },
    Probe {
        factory: Arc<dyn Factory>,
        encoder: EncoderIdentity,
        configuration: Configuration,
    },
    Open {
        factory: Arc<dyn Factory>,
        parameters: Open,
    },
    Retire {
        backend: Box<dyn Backend>,
        reply: Value,
    },
}
enum Prepared {
    Reply(Value),
    Opened {
        parameters: Open,
        backend: Box<dyn Backend>,
    },
}
struct Completion {
    waiter: Waiter,
    result: Result<Prepared, &'static str>,
    elapsed_us: u128,
}

pub(crate) struct Preparation {
    queued: VecDeque<Request>,
    sender: Option<SyncSender<(Waiter, Work)>>,
    completed: Receiver<Completion>,
    worker: Option<JoinHandle<()>>,
    active: bool,
    active_open: Option<(String, String)>,
    ready: Option<Completion>,
}

impl Preparation {
    pub fn new(notifier: WorkNotifier) -> ModuleResult<Self> {
        let (sender, incoming) = mpsc::sync_channel::<(Waiter, Work)>(1);
        let (results, completed) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("encoder-preparation".into())
            .spawn(move || {
                while let Ok((waiter, work)) = incoming.recv() {
                    let started = Instant::now();
                    let result = execute(work);
                    if results
                        .send(Completion {
                            waiter,
                            result,
                            elapsed_us: started.elapsed().as_micros(),
                        })
                        .is_err()
                    {
                        break;
                    }
                    notifier.notify();
                }
            })
            .map_err(|_| "encoder preparation worker unavailable")?;
        Ok(Self {
            queued: VecDeque::new(),
            sender: Some(sender),
            completed,
            worker: Some(worker),
            active: false,
            active_open: None,
            ready: None,
        })
    }

    pub fn admit(
        &mut self,
        id: u64,
        owner: String,
        operation: Operation,
    ) -> Result<(), &'static str> {
        if self.sender.is_none() {
            return Err("encoder_shutting_down");
        }
        if self.queued.len() + usize::from(self.active) + usize::from(self.ready.is_some())
            >= MAX_PENDING
        {
            return Err("encoder_preparation_limit");
        }
        let open_operation = match &operation {
            Operation::Open { operation_id, .. } => operation_id.clone(),
            _ => None,
        };
        self.queued.push_back(Request {
            waiter: Waiter {
                id,
                owner,
                open_operation,
            },
            operation,
        });
        Ok(())
    }

    pub fn advance(
        &mut self,
        runtime: &RuntimeClient,
        shared: &Shared,
        closing: &mut Closing,
        shutting_down: bool,
    ) -> ModuleResult<()> {
        if self.active {
            match self.completed.try_recv() {
                Ok(completion) => {
                    self.active = false;
                    self.active_open = None;
                    self.ready = Some(completion);
                }
                Err(TryRecvError::Empty) => (),
                Err(TryRecvError::Disconnected) => {
                    return Err("encoder preparation worker failed".into());
                }
            }
        }
        self.finish_completed(runtime, shared, closing, shutting_down)?;
        self.start_queued(runtime, shared, shutting_down)
    }

    fn finish_completed(
        &mut self,
        runtime: &RuntimeClient,
        shared: &Shared,
        closing: &mut Closing,
        shutting_down: bool,
    ) -> ModuleResult<()> {
        if let Some(mut completion) = self.ready.take() {
            let valid = match request_live(runtime, completion.waiter.id) {
                Ok(valid) => valid,
                Err(error) => {
                    self.ready = Some(completion);
                    return Err(error);
                }
            };
            let mut created = None;
            let payload = match completion.result {
                Ok(Prepared::Opened {
                    parameters,
                    backend,
                }) => {
                    let mut backend = Some(backend);
                    let result = if valid && !shutting_down {
                        let mut sessions = match shared.try_lock() {
                            Ok(sessions) => sessions,
                            Err(std::sync::TryLockError::WouldBlock) => {
                                completion.result = Ok(Prepared::Opened {
                                    parameters,
                                    backend: backend.take().unwrap(),
                                });
                                self.ready = Some(completion);
                                return Ok(());
                            }
                            Err(_) => return Err("encoder state failed".into()),
                        };
                        let previous = sessions.latest_id();
                        let result = sessions.finish_open(
                            &completion.waiter.owner,
                            parameters.encoder,
                            parameters.configuration,
                            parameters.operation,
                            parameters.packet_boundaries,
                            &mut backend,
                        );
                        created =
                            (sessions.latest_id() != previous).then_some(sessions.latest_id());
                        result
                    } else {
                        Err("encoder_shutting_down")
                    };
                    let payload = result.unwrap_or_else(error_reply);
                    if let Some(backend) = backend {
                        // Cancellation and rejected admission must retire the new
                        // backend on the worker before its request is settled.
                        self.dispatch(
                            completion.waiter,
                            Work::Retire {
                                backend,
                                reply: payload,
                            },
                        )?;
                        return Ok(());
                    }
                    payload
                }
                Ok(Prepared::Reply(payload)) => payload,
                Err(code) => error_reply(code),
            };
            if completion.elapsed_us >= 100_000 {
                let _ = runtime.log(
                    LogLevel::Info,
                    "encoder.preparation",
                    &format!(
                        "request={} elapsed_us={} scope=background_preparation_not_host_tick",
                        completion.waiter.id, completion.elapsed_us
                    ),
                );
            }
            if valid
                && !matches!(
                    runtime.send(Command::Complete {
                        request: completion.waiter.id,
                        payload
                    }),
                    Ok(Response::Ok)
                )
                && let Some(session) = created
            {
                closing.rollback_open(&completion.waiter.owner, session);
            }
        }
        Ok(())
    }

    fn start_queued(
        &mut self,
        runtime: &RuntimeClient,
        shared: &Shared,
        shutting_down: bool,
    ) -> ModuleResult<()> {
        if !self.active && self.ready.is_none() {
            if let Some(request) = self.queued.pop_front() {
                if !request_live(runtime, request.waiter.id)? {
                    return Ok(());
                }
                if shutting_down {
                    let _ = runtime.send(Command::Complete {
                        request: request.waiter.id,
                        payload: error_reply("encoder_shutting_down"),
                    });
                    return Ok(());
                }
                let sessions = match shared.try_lock() {
                    Ok(sessions) => sessions,
                    Err(std::sync::TryLockError::WouldBlock) => {
                        self.queued.push_front(request);
                        return Ok(());
                    }
                    Err(_) => return Err("encoder state failed".into()),
                };
                let prepared = prepare(&sessions, &request.waiter.owner, request.operation);
                drop(sessions);
                match prepared {
                    Ok(Admission::Work(work)) => self.dispatch(request.waiter, work)?,
                    result => {
                        self.ready = Some(Completion {
                            waiter: request.waiter,
                            result: match result {
                                Ok(Admission::Reply(reply)) => Ok(Prepared::Reply(reply)),
                                Err(code) => Err(code),
                                _ => unreachable!(),
                            },
                            elapsed_us: 0,
                        });
                    }
                }
            } else if shutting_down {
                self.sender.take();
            }
        }
        Ok(())
    }

    /// A lookup for an admitted open must follow its preparation and native
    /// retirement; absence is only authoritative once that owned work settles.
    pub fn has_pending_open(&self, owner: &str, operation: &str) -> bool {
        let matches = |waiter: &Waiter| {
            waiter.owner == owner && waiter.open_operation.as_deref() == Some(operation)
        };
        self.active_open
            .as_ref()
            .is_some_and(|(pending_owner, pending_operation)| {
                pending_owner == owner && pending_operation == operation
            })
            || self
                .ready
                .as_ref()
                .is_some_and(|completion| matches(&completion.waiter))
            || self.queued.iter().any(|request| matches(&request.waiter))
    }

    fn dispatch(&mut self, waiter: Waiter, work: Work) -> ModuleResult<()> {
        // A single outstanding task makes this bounded send nonblocking. A
        // disconnected worker is fatal; no native task is silently detached.
        self.active_open = waiter
            .open_operation
            .as_ref()
            .map(|operation| (waiter.owner.clone(), operation.clone()));
        self.sender
            .as_ref()
            .ok_or("encoder preparation stopped")?
            .send((waiter, work))
            .map_err(|_| "encoder preparation worker failed")?;
        self.active = true;
        Ok(())
    }

    #[cfg(test)]
    pub fn queued_empty_for_test(&self) -> bool {
        self.queued.is_empty() && !self.active && self.ready.is_none()
    }

    pub fn retired(&self) -> bool {
        self.sender.is_none()
            && !self.active
            && self.ready.is_none()
            && self.queued.is_empty()
            && self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Preparation {
    pub fn join(&mut self) {
        // Normal quiescence has already finished the worker. This final barrier
        // also protects module code/runtime pointers during exceptional unload.
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Some(completion) = self.ready.take() {
            retire_completion(completion);
        }
        while let Ok(completion) = self.completed.try_recv() {
            retire_completion(completion);
        }
    }
}

fn retire_completion(completion: Completion) {
    if let Ok(Prepared::Opened { backend, .. }) = completion.result {
        let _ = retire_backend(backend);
    }
}
fn request_live(runtime: &RuntimeClient, id: u64) -> ModuleResult<bool> {
    match runtime.send(Command::CheckRequest { request: id })? {
        Response::Ok => Ok(true),
        Response::Error { code, .. }
            if matches!(
                code.as_str(),
                "not_found" | "cancelled" | "request_ended" | "deadline_exceeded"
            ) =>
        {
            Ok(false)
        }
        response => Err(format!(
            "encoder preparation request check failed: {response:?}"
        )),
    }
}
fn error_reply(code: &'static str) -> Value {
    json!({"ok":false,"code":code})
}

fn prepare(
    sessions: &crate::sessions::Sessions,
    owner: &str,
    operation: Operation,
) -> Result<Admission, &'static str> {
    Ok(Admission::Work(match operation {
        Operation::Enumerate => Work::Enumerate {
            factory: sessions.factory(),
            enabled: sessions.enabled,
        },
        Operation::Probe {
            encoder,
            configuration,
        } => Work::Probe {
            factory: sessions.probe_factory(owner, &encoder, &configuration)?,
            encoder,
            configuration,
        },
        Operation::Open {
            encoder,
            configuration,
            operation_id,
            packet_boundaries,
        } => {
            if let Some(reply) = sessions.prepare_open(
                owner,
                &encoder,
                &configuration,
                operation_id.as_deref(),
                packet_boundaries,
            )? {
                return Ok(Admission::Reply(reply));
            }
            Work::Open {
                factory: sessions.factory(),
                parameters: Open {
                    encoder,
                    configuration,
                    operation: operation_id,
                    packet_boundaries,
                },
            }
        }
        Operation::Lookup { operation_id } => {
            return sessions.lookup(owner, &operation_id).map(Admission::Reply);
        }
        _ => return Err("invalid_request"),
    }))
}

fn execute(work: Work) -> Result<Prepared, &'static str> {
    match work {
        Work::Enumerate { factory, enabled } => {
            let encoders = if enabled {
                factory.enumerate()?
            } else {
                Vec::new()
            };
            let diagnostic = platform::last_failure().map(|detail| json!({"stage":detail.stage,"hresult":format!("0x{:08x}", detail.hresult)}));
            let metrics = platform::metrics();
            let software_unavailable = platform::software_availability().err();
            Ok(Prepared::Reply(
                json!({"ok":true,"enabled":enabled,"encoders":encoders,"diagnostic":diagnostic,
                "software_encoder":{"available":software_unavailable.is_none(),"reason":software_unavailable},
                "metrics":{"need_input_events":metrics.need_input_events,"have_output_events":metrics.have_output_events,
                "other_events":metrics.other_events,"process_input_calls":metrics.process_input_calls,"process_output_calls":metrics.process_output_calls}}),
            ))
        }
        Work::Probe {
            factory,
            encoder,
            configuration,
        } => probe::run(&*factory, encoder, configuration).map(Prepared::Reply),
        Work::Open {
            factory,
            parameters,
        } => {
            let backend =
                factory.open(parameters.encoder.clone(), parameters.configuration.clone())?;
            Ok(Prepared::Opened {
                parameters,
                backend,
            })
        }
        Work::Retire { backend, reply } => {
            retire_backend(backend)?;
            Ok(Prepared::Reply(reply))
        }
    }
}

impl Drop for Preparation {
    fn drop(&mut self) {
        self.join();
    }
}

fn retire_backend(mut backend: Box<dyn Backend>) -> Result<(), &'static str> {
    backend.begin_close();
    while !backend.ready_to_close() {
        thread::sleep(Duration::from_millis(1));
    }
    backend.close().map_err(|_| "encoder_shutdown_failed")
}
