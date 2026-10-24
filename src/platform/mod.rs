//! Platform encoder selection with an explicitly opted-in software fallback.
mod x264;

#[cfg(windows)]
mod windows;

use crate::model::{Configuration, EncoderDescription, EncoderIdentity, Packet};

#[derive(Clone, Copy)]
pub struct Diagnostic {
    pub stage: &'static str,
    pub hresult: u32,
}

#[derive(Clone, Copy)]
pub struct Metrics {
    pub need_input_events: u64,
    pub have_output_events: u64,
    pub other_events: u64,
    pub process_input_calls: u64,
    pub process_output_calls: u64,
}

pub fn last_failure() -> Option<Diagnostic> {
    #[cfg(windows)]
    {
        return windows::last_failure().map(|detail| Diagnostic {
            stage: detail.stage,
            hresult: detail.hresult,
        });
    }
    #[cfg(not(windows))]
    None
}

pub fn metrics() -> Metrics {
    #[cfg(windows)]
    {
        let metrics = windows::metrics();
        return Metrics {
            need_input_events: metrics.need_input_events,
            have_output_events: metrics.have_output_events,
            other_events: metrics.other_events,
            process_input_calls: metrics.process_input_calls,
            process_output_calls: metrics.process_output_calls,
        };
    }
    #[cfg(not(windows))]
    Metrics {
        need_input_events: 0,
        have_output_events: 0,
        other_events: 0,
        process_input_calls: 0,
        process_output_calls: 0,
    }
}

pub fn enumerate() -> Result<Vec<EncoderDescription>, &'static str> {
    let software = x264::description();
    #[cfg(windows)]
    let mut encoders = match windows::enumerate() {
        Ok(encoders) => encoders,
        Err(error) if software.is_none() => return Err(error),
        Err(_) => Vec::new(),
    };
    #[cfg(not(windows))]
    let mut encoders = Vec::new();

    if let Some(software) = software {
        encoders.push(software);
    }
    Ok(encoders)
}

pub enum Encoder {
    #[cfg(windows)]
    Hardware(windows::Encoder),
    Software(x264::Encoder),
}

impl Encoder {
    pub fn open(
        identity: &EncoderIdentity,
        configuration: &Configuration,
    ) -> Result<Self, &'static str> {
        if x264::selected(identity) {
            return x264::open(identity, configuration).map(Self::Software);
        }

        #[cfg(windows)]
        {
            windows::Encoder::open(identity, configuration).map(Self::Hardware)
        }
        #[cfg(not(windows))]
        {
            let _ = configuration;
            Err("unsupported_platform")
        }
    }

    pub fn submit(
        &mut self,
        bytes: Vec<u8>,
        timestamp: i64,
        force: bool,
    ) -> Result<(), &'static str> {
        match self {
            #[cfg(windows)]
            Self::Hardware(encoder) => encoder.submit(bytes, timestamp, force),
            Self::Software(encoder) => encoder.submit(bytes, timestamp, force),
        }
    }

    /// Software x264 runs on the dedicated worker without blocking the caller.
    /// Windows MFT keeps the existing synchronous behavior.
    pub fn uses_background_encode(&self) -> bool {
        match self {
            #[cfg(windows)]
            Self::Hardware(_) => false,
            Self::Software(_) => true,
        }
    }

    pub fn receive(&mut self) -> Result<Option<Packet>, &'static str> {
        match self {
            #[cfg(windows)]
            Self::Hardware(encoder) => encoder.receive(),
            Self::Software(encoder) => encoder.receive(),
        }
    }

    pub fn drain(&mut self) -> Result<(), &'static str> {
        match self {
            #[cfg(windows)]
            Self::Hardware(encoder) => encoder.drain(),
            Self::Software(encoder) => encoder.drain(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn software_identity_is_explicit_and_does_not_alias_hardware() {
        let software = x264::identity();
        assert!(x264::selected(&software));
        let mut hardware = software.clone();
        hardware.clsid = "hardware-transform".into();
        assert!(!x264::selected(&hardware));
    }

    #[test]
    fn software_encoder_is_listed_only_when_runtime_is_available() {
        let encoders = enumerate().unwrap();
        assert_eq!(
            encoders
                .iter()
                .any(|encoder| x264::selected(&encoder.identity)),
            x264::availability().is_ok()
        );
    }

    #[cfg(not(amaterasu_x264))]
    #[test]
    fn unavailable_software_encoder_fails_closed() {
        let mut configuration: Configuration = serde_json::from_value(serde_json::json!({
            "width": 16,
            "height": 16,
            "fps": 60,
            "bitrate": 1_000_000
        }))
        .unwrap();
        assert!(matches!(
            Encoder::open(&x264::identity(), &configuration),
            Err("x264_feature_disabled" | "x264_headers_unavailable")
        ));
        configuration.width = 0;
        assert!(matches!(
            Encoder::open(&x264::identity(), &configuration),
            Err("x264_feature_disabled" | "x264_headers_unavailable")
        ));
    }
}
