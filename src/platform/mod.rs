#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as hardware;
mod x264;
pub use x264::availability as software_availability;
#[cfg(not(windows))]
mod unsupported {
    use crate::model::{Configuration, EncoderDescription, EncoderIdentity, Packet};
    #[derive(Clone, Copy)]
    pub struct Diagnostic {
        pub stage: &'static str,
        pub hresult: u32,
    }
    pub fn enumerate() -> Result<Vec<EncoderDescription>, &'static str> {
        Ok(Vec::new())
    }
    pub struct Encoder;
    pub fn last_failure() -> Option<Diagnostic> {
        None
    }
    #[derive(Clone, Copy)]
    pub struct Metrics {
        pub need_input_events: u64,
        pub have_output_events: u64,
        pub other_events: u64,
        pub process_input_calls: u64,
        pub process_output_calls: u64,
    }
    pub fn metrics() -> Metrics {
        Metrics {
            need_input_events: 0,
            have_output_events: 0,
            other_events: 0,
            process_input_calls: 0,
            process_output_calls: 0,
        }
    }
    impl Encoder {
        pub fn open(_: &EncoderIdentity, _: &Configuration) -> Result<Self, &'static str> {
            Err("unsupported_platform")
        }
        pub fn submit(&mut self, _: Vec<u8>, _: i64, _: bool) -> Result<(), &'static str> {
            Err("unsupported_platform")
        }
        pub fn receive(&mut self) -> Result<Option<Packet>, &'static str> {
            Err("unsupported_platform")
        }
        pub fn drain(&mut self) -> Result<(), &'static str> {
            Err("unsupported_platform")
        }
    }
}

pub use hardware::{last_failure, metrics};
#[cfg(not(windows))]
use unsupported as hardware;

use crate::model::{Configuration, EncoderDescription, EncoderIdentity, Packet};
use amaterasu_module_sdk::LogEmitter;

pub fn enumerate() -> Result<Vec<EncoderDescription>, &'static str> {
    let software = x264::description();
    let mut encoders = match hardware::enumerate() {
        Ok(encoders) => encoders,
        // A missing hardware device must not hide an independently available
        // software backend; diagnostics remain visible in enumeration output.
        Err(_) if software.is_some() => Vec::new(),
        Err(error) => return Err(error),
    };
    encoders.extend(software);
    Ok(encoders)
}

pub enum Encoder {
    Hardware(Box<hardware::Encoder>),
    Software(Box<x264::Encoder>),
}

impl Encoder {
    pub fn open(identity: &EncoderIdentity, config: &Configuration) -> Result<Self, &'static str> {
        Self::open_with_log_emitter(identity, config, None)
    }

    pub fn open_with_log_emitter(
        identity: &EncoderIdentity,
        config: &Configuration,
        emitter: Option<LogEmitter>,
    ) -> Result<Self, &'static str> {
        if x264::selected(identity) {
            x264::Encoder::open_with_log_emitter(identity, config, emitter)
                .map(|encoder| Self::Software(Box::new(encoder)))
        } else {
            hardware::Encoder::open(identity, config)
                .map(|encoder| Self::Hardware(Box::new(encoder)))
        }
    }
    pub fn submit(
        &mut self,
        bytes: Vec<u8>,
        timestamp: i64,
        force: bool,
    ) -> Result<(), &'static str> {
        match self {
            Self::Hardware(encoder) => encoder.submit(bytes, timestamp, force),
            Self::Software(encoder) => encoder.submit(bytes, timestamp, force),
        }
    }
    pub fn receive(&mut self) -> Result<Option<Packet>, &'static str> {
        match self {
            Self::Hardware(encoder) => encoder.receive(),
            Self::Software(encoder) => encoder.receive(),
        }
    }
    pub fn drain(&mut self) -> Result<(), &'static str> {
        match self {
            Self::Hardware(encoder) => encoder.drain(),
            Self::Software(encoder) => encoder.drain(),
        }
    }
}
