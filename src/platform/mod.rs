#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{Encoder, enumerate, last_failure};
#[cfg(not(windows))]
mod unsupported {
    use crate::model::{Configuration, EncoderDescription, Packet};
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
    impl Encoder {
        pub fn open(_: u32, _: &Configuration) -> Result<Self, &'static str> {
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
pub use unsupported::{Encoder, enumerate, last_failure};
