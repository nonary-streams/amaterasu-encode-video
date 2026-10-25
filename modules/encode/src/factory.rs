//! Backend activation belongs to this factory; sessions and probes own its results.
use crate::{
    model::{Configuration, EncoderDescription, EncoderIdentity},
    sessions::Backend,
    worker::{self, Worker},
};
use amaterasu_module_sdk::LogEmitter;

pub trait Factory: Send + Sync {
    fn enumerate(&self) -> Result<Vec<EncoderDescription>, &'static str>;
    /// Activation errors transfer no live backend. `encoder_shutdown_failed`
    /// is an ambiguous native teardown and must never authorize Auto fallback.
    fn open(
        &self,
        encoder: EncoderIdentity,
        configuration: Configuration,
    ) -> Result<Box<dyn Backend>, &'static str>;
}

pub struct NativeFactory;

pub struct LogForwardingFactory {
    emitter: LogEmitter,
}

impl LogForwardingFactory {
    pub fn new(emitter: LogEmitter) -> Self {
        Self { emitter }
    }
}

impl Factory for NativeFactory {
    fn enumerate(&self) -> Result<Vec<EncoderDescription>, &'static str> {
        worker::enumerate()
    }

    fn open(
        &self,
        encoder: EncoderIdentity,
        configuration: Configuration,
    ) -> Result<Box<dyn Backend>, &'static str> {
        Worker::open(encoder, configuration).map(|worker| Box::new(worker) as Box<dyn Backend>)
    }
}

impl Factory for LogForwardingFactory {
    fn enumerate(&self) -> Result<Vec<EncoderDescription>, &'static str> {
        worker::enumerate()
    }

    fn open(
        &self,
        encoder: EncoderIdentity,
        configuration: Configuration,
    ) -> Result<Box<dyn Backend>, &'static str> {
        Worker::open_with_log_emitter(encoder, configuration, self.emitter.clone())
            .map(|worker| Box::new(worker) as Box<dyn Backend>)
    }
}
