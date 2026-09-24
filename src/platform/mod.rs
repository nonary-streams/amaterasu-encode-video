#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{Encoder, enumerate, last_failure, metrics};
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

#[cfg(not(windows))]
pub use unsupported::{Encoder, enumerate, last_failure, metrics};
