//! Owned KWin virtual outputs prefer DMA-BUF capture through capture-vkms and
//! keep PipeWire CPU capture as their fallback. Losing the GPU path never fails
//! a stream that PipeWire can still serve, and every abandoned GPU path records
//! why in stream status.
use super::*;
use crate::plane::FramePath;

/// Longest provider-reported failure code retained in status.
const MAX_REASON_BYTES: usize = 64;

#[derive(Default)]
pub(super) struct OwnedOutputCapture {
    /// PipeWire capture of an owned KWin output, or explicitly pinned vkms capture.
    candidate: bool,
    /// `vkms` is in the startup provider allowlist.
    vkms_allowed: bool,
    /// The provider choice still runs before the first capture open.
    selection_pending: bool,
    /// Whether the selected encoder lists "dmabuf" among its inputs. `None`
    /// when an explicit identity skipped the enumeration that reports inputs.
    encoder_accepts_dmabuf: Option<bool>,
    /// The CPU provider still available after leaving vkms; used at most once.
    cpu_provider: Option<CaptureProvider>,
    fallback_reason: Option<String>,
    fresh_frames: u64,
    reused_frames: u64,
}

impl OwnedOutputCapture {
    pub(super) fn new(provider: CaptureProvider, output: &str, vkms_allowed: bool) -> Self {
        let candidate = match provider {
            CaptureProvider::Pipewire => is_owned_kwin_output(output),
            CaptureProvider::Vkms => true,
            CaptureProvider::Dxgi
            | CaptureProvider::Wgc
            | CaptureProvider::X11
            | CaptureProvider::Kms => false,
        };
        let falls_back_to_pipewire = candidate && provider == CaptureProvider::Pipewire;
        Self {
            candidate,
            vkms_allowed,
            selection_pending: candidate,
            cpu_provider: falls_back_to_pipewire.then_some(CaptureProvider::Pipewire),
            ..Self::default()
        }
    }

    /// Encoder selection ranks DMA-BUF-capable encoders first only when the
    /// capture side could actually deliver DMA-BUF frames to them.
    pub(super) fn prefers_dmabuf(&self, plane: &dyn Plane) -> bool {
        self.candidate
            && self.vkms_allowed
            && plane.dmabuf_unavailable(CaptureProvider::Vkms).is_none()
    }

    pub(super) fn record_encoder_inputs(&mut self, accepts_dmabuf: bool) {
        self.encoder_accepts_dmabuf = Some(accepts_dmabuf);
    }

    fn unavailable_reason(&self, plane: &dyn Plane) -> Option<&'static str> {
        if !self.vkms_allowed {
            return Some("capture_vkms_not_configured");
        }
        if let Some(reason) = plane.dmabuf_unavailable(CaptureProvider::Vkms) {
            return Some(reason);
        }
        match self.encoder_accepts_dmabuf {
            Some(true) => None,
            Some(false) => Some("encoder_dmabuf_input_unavailable"),
            None => Some("encoder_inputs_unknown"),
        }
    }

    pub(super) fn record_freshness(&mut self, progress: &Progress) {
        match progress.dmabuf_fresh {
            Some(true) => self.fresh_frames = self.fresh_frames.saturating_add(1),
            Some(false) => self.reused_frames = self.reused_frames.saturating_add(1),
            None => {}
        }
    }

    pub(super) fn write_status(&self, response: &mut Value) {
        if !self.candidate {
            return;
        }
        response["vkms_fallback_reason"] = json!(self.fallback_reason);
        response["dmabuf_frames"] = json!({
            "fresh": self.fresh_frames,
            "reused": self.reused_frames,
        });
    }
}

/// Chooses vkms DMA-BUF capture before the first capture open when the vkms
/// binding carries capture v1.4, the encoder binding carries v1.1, and the
/// selected encoder accepts DMA-BUF input. Otherwise the stream keeps PipeWire
/// and records the reason; pinned vkms streams fail with that reason instead.
pub(super) fn select_capture_provider(stream: &mut Stream, plane: &dyn Plane) {
    if !stream.owned_capture.selection_pending {
        return;
    }
    stream.owned_capture.selection_pending = false;
    match stream.owned_capture.unavailable_reason(plane) {
        None => {
            stream.provider = CaptureProvider::Vkms;
            stream.frame_path = FramePath::DmaBuf;
        }
        Some(reason) if stream.provider == CaptureProvider::Vkms => stream.cleanup(reason),
        Some(reason) => {
            stream.owned_capture.cpu_provider = None;
            stream.owned_capture.fallback_reason = Some(reason.into());
        }
    }
}

/// Retries an owned output through PipeWire after vkms definitively refused to
/// open it. Uncertain or lost replies never reach here because they may have
/// opened a stream that cleanup must still find.
pub(super) fn begin_vkms_open_fallback(stream: &mut Stream, reason: &str) -> bool {
    if stream.provider != CaptureProvider::Vkms || stream.session.is_some() {
        return false;
    }
    let Some(provider) = stream.owned_capture.cpu_provider.take() else {
        return false;
    };
    stream.owned_capture.fallback_reason = Some(retained_reason(reason));
    stream.frame_path = FramePath::Cpu;
    switch_capture_provider(stream, provider);
    // A refused open holds no capture and the encoder opens only afterwards.
    stream.no_resources = true;
    true
}

/// Leaves the DMA-BUF path once, after either native side reported that this
/// stream cannot exchange GPU frames. Video closes the vkms capture, opens the
/// CPU provider for the same output, and then resumes the open encoder session.
pub(super) fn begin_cpu_capture_fallback(stream: &mut Stream, reason: &'static str) -> bool {
    if stream.stage != Stage::Running
        || stream.frame_path != FramePath::DmaBuf
        || stream.owned_capture.cpu_provider.is_none()
    {
        return false;
    }
    stream.owned_capture.fallback_reason = Some(reason.into());
    stream.frame_path = FramePath::Cpu;
    stream.restart_pending = true;
    stream.force_idr = true;
    stream.last_pixels = None;
    stream.next_repeat_at = None;
    stream.retries = 0;
    stream.stage = Stage::CpuFallbackCloseCapture;
    true
}

/// The vkms capture is confirmed closed: open the CPU provider in its place.
pub(super) fn reopen_cpu_capture(stream: &mut Stream) {
    match stream.owned_capture.cpu_provider.take() {
        Some(provider) => switch_capture_provider(stream, provider),
        None => stream.cleanup("capture_fallback_unavailable"),
    }
}

fn switch_capture_provider(stream: &mut Stream, provider: CaptureProvider) {
    stream.provider = provider;
    stream.capture = None;
    stream.capture_attempt = stream.capture_attempt.saturating_add(1);
    stream.capture_operation = format!("{}-capture-{}", stream.operation, stream.capture_attempt);
    stream.stage = Stage::Capture;
    stream.retries = 0;
}

/// Keeps provider-reported codes that are short identifiers; anything else is
/// summarized so status never echoes arbitrary provider text.
pub(super) fn retained_reason(reason: &str) -> String {
    let identifier = !reason.is_empty()
        && reason.len() <= MAX_REASON_BYTES
        && reason
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    if identifier {
        reason.into()
    } else {
        "provider_failed".into()
    }
}
