//! Stream failures this encoder reports first-hand.
//!
//! The video service opens each encoder session with an `operation_id` it
//! derived from its own stream operation and bound to the launch. Failures of
//! that open, and later data-plane failures of the session it created, are
//! reported under that operation with this module as the publisher.
use crate::model::EncoderIdentity;
use amaterasu_module_sdk::stream_lifecycle::{
    LifecycleLayer, StreamIssue, StreamIssueOutbox, StreamReport,
};

/// A short identifier for the backend an encoder identity selects.
pub(crate) fn backend_name(encoder: &EncoderIdentity) -> &'static str {
    let clsid = encoder.clsid.as_str();
    if clsid.starts_with("amaterasu.hardware.nvenc-cuda.") {
        "nvenc"
    } else if clsid.starts_with("amaterasu.hardware.nvidia.nvenc.") {
        "ffmpeg_nvenc"
    } else if clsid.starts_with("amaterasu.hardware.vaapi.") {
        "vaapi"
    } else if clsid.starts_with("amaterasu.software.x264") {
        "x264"
    } else if clsid.starts_with("amaterasu.software.linux.x265.") {
        "x265"
    } else if clsid == "pyrowave" {
        "pyrowave"
    } else if encoder.hardware_url.is_some() {
        "hardware_mft"
    } else {
        "encoder"
    }
}

/// An encoder session the stream depended on failed in `step`.
#[track_caller]
pub(crate) fn session_failure(
    operation: &str,
    backend: &str,
    step: &str,
    code: &str,
) -> StreamIssue {
    StreamIssue::critical(
        operation,
        LifecycleLayer::EncodeVideo,
        format!("{backend}.{step}"),
        code,
    )
}

/// Move queued reports between outboxes, keeping their order.
pub(crate) fn transfer(from: &mut StreamIssueOutbox, to: &mut StreamIssueOutbox) {
    for report in from.drain() {
        match report {
            StreamReport::Issue(issue) => to.push(issue),
            StreamReport::Binding(binding) => to.push_binding(binding),
        }
    }
}
