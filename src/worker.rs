//! Dedicated threads own native codec resources; no native pointer crosses threads.
use crate::{
    model::{Configuration, EncoderDescription, EncoderIdentity, Packet},
    platform,
};
use std::{
    sync::mpsc::{self, SyncSender},
    thread::{self, JoinHandle},
    time::Duration,
};

enum Operation {
    Submit(Vec<u8>, i64, bool),
    Receive,
    Drain,
    Close,
}

enum Reply {
    Done,
    Packet(Option<Packet>),
}

struct Request {
    operation: Operation,
    reply: SyncSender<Result<Reply, &'static str>>,
}

pub struct Worker {
    sender: Option<SyncSender<Request>>,
    thread: Option<JoinHandle<()>>,
    failed: bool,
}

impl Worker {
    pub fn open(identity: EncoderIdentity, config: Configuration) -> Result<Self, &'static str> {
        let (sender, receiver) = mpsc::sync_channel::<Request>(1);
        let (ready, initialized) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("amaterasu-h264".into())
            .spawn(move || {
                let mut encoder = match platform::Encoder::open(&identity, &config) {
                    Ok(encoder) => encoder,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                if ready.send(Ok(())).is_err() {
                    return;
                }
                while let Ok(request) = receiver.recv() {
                    let result = match request.operation {
                        Operation::Submit(data, timestamp, force) => {
                            encoder.submit(data, timestamp, force).map(|()| Reply::Done)
                        }
                        Operation::Receive => encoder.receive().map(Reply::Packet),
                        Operation::Drain => encoder.drain().map(|()| Reply::Done),
                        Operation::Close => break,
                    };
                    if request.reply.send(result).is_err() {
                        break;
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
            Ok(()) => Ok(Self {
                sender: Some(sender),
                thread: Some(thread),
                failed: false,
            }),
            Err(error) => {
                drop(sender);
                thread.join().map_err(|_| "encoder_shutdown_failed")?;
                Err(error)
            }
        }
    }
    fn request(&mut self, operation: Operation) -> Result<Reply, &'static str> {
        if self.failed {
            return Err("encoder_failed");
        }
        let (reply, response) = mpsc::sync_channel(1);
        self.sender
            .as_ref()
            .ok_or("encoder_worker_failed")?
            .try_send(Request { operation, reply })
            .map_err(|_| "encoder_worker_failed")?;
        let result = response
            .recv_timeout(Duration::from_secs(3))
            .map_err(|_| "encoder_timeout")
            .and_then(|value| value);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    pub fn submit(
        &mut self,
        data: Vec<u8>,
        timestamp: i64,
        force: bool,
    ) -> Result<(), &'static str> {
        self.request(Operation::Submit(data, timestamp, force))
            .map(|_| ())
    }
    pub fn receive(&mut self) -> Result<Option<Packet>, &'static str> {
        match self.request(Operation::Receive)? {
            Reply::Packet(packet) => Ok(packet),
            _ => Err("encoder_worker_failed"),
        }
    }
    pub fn drain(&mut self) -> Result<(), &'static str> {
        self.request(Operation::Drain).map(|_| ())
    }
    pub fn shutdown(mut self) -> Result<(), &'static str> {
        self.close_thread()
    }
    fn close_thread(&mut self) -> Result<(), &'static str> {
        let (reply, _) = mpsc::sync_channel(1);
        if let Some(sender) = self.sender.take() {
            let _ = sender.try_send(Request {
                operation: Operation::Close,
                reply,
            });
        }
        if let Some(thread) = self.thread.take() {
            thread.join().map_err(|_| "encoder_shutdown_failed")?;
        }
        Ok(())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Native driver calls cannot safely be forcibly interrupted. A healthy
        // worker releases COM synchronously; a hung worker must keep its code
        // module loaded, so join intentionally waits rather than detaching it.
        let _ = self.close_thread();
    }
}

pub fn enumerate() -> Result<Vec<EncoderDescription>, &'static str> {
    thread::spawn(platform::enumerate)
        .join()
        .map_err(|_| "encoder_worker_failed")?
}
