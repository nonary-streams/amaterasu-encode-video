//! Capture format negotiation is bound to the signed logical route, independently
//! of encoder codec and output range. Legacy opens do not gain new JSON fields.
use crate::{
    model::{CaptureProvider, Configuration, SourcePreflightRoute},
    plane::Plane,
};
use amaterasu_media::{MEDIA_BUSY, MEDIA_PENDING};
use amaterasu_module_sdk::{
    DynamicRange,
    capture::{SourceCaptureContext, SourceFormat, SourceMode, SourcePreflightRequest},
};
use std::collections::BTreeMap;

pub(super) struct Selection {
    pub(super) context: SourceCaptureContext,
    route: SourcePreflightRoute,
    output: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum FormatProgress {
    Pending,
    Ready(Option<SourceFormat>),
}

pub(super) fn select(
    context: Option<&SourceCaptureContext>,
    provider: CaptureProvider,
    output: &str,
    routes: &BTreeMap<String, SourcePreflightRoute>,
) -> Result<Option<Selection>, &'static str> {
    let Some(context) = context else {
        return Ok(None);
    };
    context
        .validate()
        .map_err(|_| "invalid_source_capture_context")?;
    let route = *routes
        .get(&context.route_key)
        .ok_or("source_route_unselected")?;
    if route.provider != provider {
        return Err("source_route_provider_mismatch");
    }
    Ok(Some(Selection {
        context: context.clone(),
        route,
        output: output.into(),
    }))
}

fn required_format(configuration: &Configuration) -> SourceFormat {
    let hdr = configuration.gpu_processing.as_ref().map_or(
        configuration.video_stream_format.dynamic_range == DynamicRange::Hdr,
        |processing| processing.source_hdr_required,
    );
    SourceFormat {
        bit_depth: if hdr {
            10
        } else {
            configuration.video_stream_format.bit_depth
        },
        dynamic_range: if hdr {
            DynamicRange::Hdr
        } else {
            DynamicRange::Sdr
        },
    }
}

pub(super) fn format(
    selection: Option<&Selection>,
    provider: CaptureProvider,
    output: &str,
    configuration: &Configuration,
    plane: &mut dyn Plane,
) -> Result<FormatProgress, &'static str> {
    let format = required_format(configuration);
    format
        .validate()
        .map_err(|_| "invalid_capture_source_format")?;
    let Some(selection) = selection else {
        return if format.bit_depth == 8 && format.dynamic_range == DynamicRange::Sdr {
            Ok(FormatProgress::Ready(None))
        } else {
            Err("capture_source_route_required")
        };
    };
    if selection.output != output {
        return Err("capture_source_selector_changed");
    }
    if selection.route.provider != provider {
        return Err("source_route_provider_mismatch");
    }
    let hdr = format.dynamic_range == DynamicRange::Hdr;
    let mut mode = selection.context.source_mode.unwrap_or(SourceMode {
        width: configuration.width,
        height: configuration.height,
        refresh_millihz: configuration.fps.saturating_mul(1000),
        hdr,
        bit_depth: Some(format.bit_depth),
    });
    if mode.hdr != hdr
        || mode
            .bit_depth
            .is_some_and(|depth| depth != format.bit_depth)
    {
        return Err("capture_source_format_mismatch");
    }
    mode.bit_depth = Some(format.bit_depth);
    let request = SourcePreflightRequest {
        source_class: selection.route.source_class,
        route: Some(output.into()),
        mode,
    };
    request
        .validate()
        .map_err(|_| "invalid_capture_source_mode")?;
    let response = match plane.source_preflight(provider, &request) {
        Ok(response) => response,
        Err(MEDIA_BUSY | MEDIA_PENDING) => return Ok(FormatProgress::Pending),
        Err(_) => return Err("capture_source_preflight_unavailable"),
    };
    response
        .validate(&request)
        .map_err(|_| "invalid_capture_source_preflight")?;
    if !response.is_ready() {
        return Err("capture_source_not_ready");
    }
    // A provider without format_open retains its original strict Open schema.
    // Its positive depth/range capability attests its opaque selector's format.
    Ok(FormatProgress::Ready(
        response
            .capabilities
            .iter()
            .any(|capability| capability.supports(&request) && capability.format_open)
            .then_some(format),
    ))
}
