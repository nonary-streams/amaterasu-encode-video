//! Capture providers request transitions; only this coordinator selects from
//! trusted startup registrations. Encoding configuration is never changed here.
use super::*;
use amaterasu_module_sdk::capture::{PendingSwap, SwapDecision};

pub(super) fn observe(
    stream: &mut Stream,
    id: u64,
    providers: &BTreeSet<CaptureProvider>,
    plane: &mut dyn Plane,
) {
    let result = plane.capture_notice(
        id,
        stream.provider,
        stream.capture.expect("running capture"),
    );
    let notice = match result {
        Ok(Some(notice)) => notice,
        Ok(None) => return,
        Err(code) => {
            stream.cleanup(code);
            return;
        }
    };
    stream.capture_notifications = true;
    if notice.closed {
        stream.cleanup("capture_source_stopped");
        return;
    }
    stream.capture_event_ready |= notice.frame_ready;
    let Some(request) = notice.swap_request else {
        return;
    };
    let target = select_target(stream, providers, &request);
    let decision = match &target {
        Ok(_) => SwapDecision::Accepted,
        Err(reason) => SwapDecision::Rejected {
            reason: (*reason).into(),
        },
    };
    if let Err(code) = plane.resolve_capture_swap(id, request.id, decision) {
        stream.cleanup(code);
        return;
    }
    if let Ok(target) = target {
        stream.capture_swap_target = Some(target);
        stream.restart_pending = true;
        stream.force_idr = true;
        stream.last_pixels = None;
        stream.next_repeat_at = None;
        stream.retries = 0;
        stream.stage = Stage::ReplaceCapture;
    }
}

fn select_target(
    stream: &Stream,
    providers: &BTreeSet<CaptureProvider>,
    pending: &PendingSwap,
) -> Result<(CaptureProvider, String), &'static str> {
    let request = &pending.request;
    if stream.capture_attempt == u64::MAX {
        return Err("capture_swap_limit");
    }
    request.validate().map_err(|_| "invalid_capture_swap")?;
    // The existing GPU cancellation contract retires an encoder/processor
    // stream permanently. Reusing it after a capture change needs a separate
    // pipeline restart, deliberately outside this capture-only API increment.
    if stream.frame_path != FramePath::Cpu {
        return Err("capture_swap_requires_pipeline_restart");
    }
    if stream.constant_rate {
        return Err("capture_swap_requires_mode_restart");
    }
    // Output IDs are provider-specific. Never guess a desktop/output mapping.
    let output = request
        .output
        .clone()
        .ok_or("capture_swap_output_required")?;
    let target = providers
        .iter()
        .copied()
        .find(|provider| {
            *provider != stream.provider
                && request
                    .preferred_provider
                    .as_deref()
                    .is_none_or(|alias| provider.identity() == alias)
                && *provider != CaptureProvider::Vkms
                && !provider.uses_external_dmabuf()
        })
        .ok_or("capture_swap_provider_unavailable")?;
    Ok((target, output))
}

/// Invoked only after the old capture's exact close has been confirmed.
pub(super) fn reopen_requested_capture(stream: &mut Stream) -> bool {
    let Some((provider, output)) = stream.capture_swap_target.take() else {
        return false;
    };
    stream.provider = provider;
    stream.capture_output = output.clone();
    stream.output = output;
    stream.capture = None;
    stream.capture_attempt = stream.capture_attempt.saturating_add(1);
    stream.capture_operation = format!("{}-capture-{}", stream.operation, stream.capture_attempt);
    stream.owned_capture = owned_capture::OwnedOutputCapture::default();
    stream.fallback_provider = None;
    stream.fallback_attempted = true;
    stream.capture_notifications = false;
    stream.capture_event_ready = false;
    stream.presentation.deactivate();
    stream.stage = Stage::Capture;
    stream.retries = 0;
    true
}
