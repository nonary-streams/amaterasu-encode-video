//! Pure source admission against exactly the capture route selected at startup.
use crate::model::{CaptureProvider, Settings, SourcePreflightRoute, error};
use amaterasu_media::{MEDIA_BUSY, MEDIA_PENDING, MEDIA_UNSUPPORTED};
use amaterasu_module_sdk::{
    Command, RuntimeClient,
    abi::*,
    capture::{
        SourceClass, SourceMode, SourcePreflightConnection, SourcePreflightRequest,
        SourcePreflightResponse,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    PreflightSource,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Query {
    action: Action,
    route_key: String,
    source_class: SourceClass,
    route: Option<String>,
    mode: SourceMode,
}

pub(super) struct SourcePreflight {
    enabled: bool,
    routes: BTreeMap<String, SourcePreflightRoute>,
    preflight_only: BTreeSet<String>,
    bindings: BTreeMap<CaptureProvider, Option<SourcePreflightConnection>>,
}
impl SourcePreflight {
    pub fn new(settings: &Settings) -> Self {
        Self {
            enabled: settings.enabled,
            routes: settings.resolved_source_preflight_routes(),
            preflight_only: settings.preflight_only_principals.clone(),
            bindings: BTreeMap::new(),
        }
    }
    pub fn operation_allowed(&self, caller: &str, payload: &Value) -> bool {
        !self.preflight_only.contains(caller) || payload["action"] == "preflight_source"
    }
    pub fn handle(&mut self, runtime: &RuntimeClient, request_id: u64, payload: Value) -> Value {
        let (route_key, request) = match decode(payload) {
            Ok(query) => query,
            Err(code) => return error(code),
        };
        let selected = self.routes.get(&route_key).copied();
        query_selected(self.enabled, selected, &request, &mut |provider, query| {
            if runtime
                .require_success(Command::CheckRequest {
                    request: request_id,
                })
                .is_err()
            {
                return Err(MEDIA_UNSUPPORTED);
            }
            let binding = self.bindings.entry(provider).or_insert_with(|| {
                // SAFETY: signed startup settings resolve this exact selected
                // capture service; binding and runtime outlive every query.
                unsafe { SourcePreflightConnection::bind(runtime, provider.service()) }.ok()
            });
            binding.as_ref().ok_or(MEDIA_UNSUPPORTED)?.query(query)
        })
    }
}

fn decode(payload: Value) -> Result<(String, SourcePreflightRequest), &'static str> {
    let Query {
        action: Action::PreflightSource,
        route_key,
        source_class,
        route,
        mode,
    } = serde_json::from_value(payload).map_err(|_| "invalid_source_preflight_request")?;
    let request = SourcePreflightRequest {
        source_class,
        route,
        mode,
    };
    if route_key.is_empty()
        || route_key.len() > 128
        || route_key.chars().any(char::is_control)
        || request.validate().is_err()
    {
        return Err("invalid_source_preflight_request");
    }
    Ok((route_key, request))
}

fn query_selected(
    enabled: bool,
    selected: Option<SourcePreflightRoute>,
    request: &SourcePreflightRequest,
    query: &mut impl FnMut(
        CaptureProvider,
        &SourcePreflightRequest,
    ) -> Result<SourcePreflightResponse, Status>,
) -> Value {
    let response = if !enabled {
        SourcePreflightResponse::unavailable(request, "disabled")
    } else if let Some(route) = selected {
        if route.source_class != request.source_class {
            return json!({"ok":true,"preflight":SourcePreflightResponse::unsupported(request,"source_class_unselected")});
        }
        match query(route.provider, request) {
            Ok(response) => response,
            Err(MEDIA_UNSUPPORTED) => {
                SourcePreflightResponse::unsupported(request, "source_preflight_unsupported")
            }
            Err(MEDIA_BUSY | MEDIA_PENDING) => {
                SourcePreflightResponse::unavailable(request, "source_preflight_busy")
            }
            Err(_) => SourcePreflightResponse::unavailable(request, "source_preflight_failed"),
        }
    } else {
        SourcePreflightResponse::unsupported(request, "source_route_unselected")
    };
    if response.validate(request).is_err() {
        return error("invalid_source_preflight_response");
    }
    json!({"ok":true,"preflight":response})
}

#[cfg(test)]
mod tests {
    use super::*;
    use amaterasu_module_sdk::capture::{
        SourceModeCapabilities, decode_video_source_preflight, source_preflight_payload,
    };

    fn request() -> SourcePreflightRequest {
        SourcePreflightRequest {
            source_class: SourceClass::ManagedOutput,
            route: None,
            mode: SourceMode {
                width: 1920,
                height: 1080,
                refresh_millihz: 60_000,
                hdr: false,
                bit_depth: None,
            },
        }
    }
    fn ready(request: &SourcePreflightRequest) -> SourcePreflightResponse {
        SourcePreflightResponse::ready(
            request,
            vec![SourceModeCapabilities {
                source_class: request.source_class,
                min_width: 1,
                max_width: 8192,
                min_height: 1,
                max_height: 8192,
                min_refresh_millihz: 1_000,
                max_refresh_millihz: 240_000,
                sdr: true,
                hdr: true,
                bit_depths: vec![8, 10],
                format_open: false,
            }],
        )
    }
    #[test]
    fn same_video_consumer_uses_two_independently_selected_capture_providers_without_fallback() {
        for provider in [CaptureProvider::Pipewire, CaptureProvider::Vkms] {
            let mut called = Vec::new();
            let request = request();
            let reply = query_selected(
                true,
                Some(SourcePreflightRoute {
                    provider,
                    source_class: request.source_class,
                }),
                &request,
                &mut |selected, request| {
                    called.push(selected);
                    Ok(ready(request))
                },
            );
            assert!(
                decode_video_source_preflight(&reply, &request)
                    .unwrap()
                    .is_ready()
            );
            assert_eq!(called, [provider]);
            let unavailable = query_selected(
                true,
                Some(SourcePreflightRoute {
                    provider,
                    source_class: request.source_class,
                }),
                &request,
                &mut |selected, _| {
                    called.push(selected);
                    Err(MEDIA_UNSUPPORTED)
                },
            );
            assert!(
                !decode_video_source_preflight(&unavailable, &request)
                    .unwrap()
                    .is_ready()
            );
            assert_eq!(
                called,
                [provider, provider],
                "a failed selected provider never invokes an alternative"
            );
        }
    }
    #[test]
    fn only_signed_route_keys_can_select_a_provider_and_no_native_query_runs_for_unselected_routes()
    {
        let settings:Settings=serde_json::from_value(json!({"enabled":true,"capture_providers":["pipewire"],"source_preflight_routes":{"third-party.capture-route/2":{"provider":"pipewire","class":"managed_output"}}})).unwrap();
        settings.validate().unwrap();
        let registry = SourcePreflight::new(&settings);
        assert_eq!(
            registry.routes["third-party.capture-route/2"].provider,
            CaptureProvider::Pipewire
        );
        let request = request();
        let reply = query_selected(true, None, &request, &mut |_, _| {
            panic!("unselected native provider queried")
        });
        assert!(
            !decode_video_source_preflight(&reply, &request)
                .unwrap()
                .is_ready()
        );
        let mut injection =
            source_preflight_payload("third-party.capture-route/2", &request).unwrap();
        injection["service"] = json!("attacker.capture/1");
        assert!(decode(injection).is_err());
    }
    #[test]
    fn unknown_selected_alias_and_mismatched_provider_echo_are_rejected() {
        let settings:Settings=serde_json::from_value(json!({"enabled":true,"capture_providers":["pipewire"],"source_preflight_routes":{"managed_output":{"provider":"unselected","class":"managed_output"}}})).unwrap();
        assert_eq!(settings.validate(), Err("invalid_source_preflight_routes"));
        let request = request();
        let reply = query_selected(
            true,
            Some(SourcePreflightRoute {
                provider: CaptureProvider::Pipewire,
                source_class: request.source_class,
            }),
            &request,
            &mut |_, request| {
                let mut changed = request.clone();
                changed.mode.width += 1;
                Ok(ready(&changed))
            },
        );
        assert!(decode_video_source_preflight(&reply, &request).is_err());
    }

    #[test]
    fn preflight_observers_cannot_invoke_any_raw_video_operation_or_change_the_signed_class() {
        let settings:Settings=serde_json::from_value(json!({"enabled":true,"capture_providers":["pipewire"],"source_preflight_routes":{"managed_output":{"provider":"pipewire","class":"managed_output"}},"preflight_only_principals":["remote-policy"]})).unwrap();
        settings.validate().unwrap();
        let registry = SourcePreflight::new(&settings);
        assert!(registry.operation_allowed("remote-policy", &json!({"action":"preflight_source"})));
        for action in [
            "open",
            "close",
            "open_source",
            "open_with_options",
            "status",
            "lookup",
            "observe_capture",
            "force_idr",
            "drain",
            "select_portal_source",
            "poll_portal_selection",
            "cancel_portal_selection",
            "unknown",
        ] {
            assert!(
                !registry.operation_allowed("remote-policy", &json!({"action":action})),
                "observer action {action} reached raw Video handler"
            );
            assert!(
                registry.operation_allowed("ordinary-video-consumer", &json!({"action":action}))
            );
        }
        assert!(!registry.operation_allowed("remote-policy", &json!({"method":"open"})));
        let mut request = request();
        request.source_class = SourceClass::DesktopOutput;
        let response = query_selected(
            true,
            registry.routes.get("managed_output").copied(),
            &request,
            &mut |_, _| panic!("caller changed the signed source class"),
        );
        assert!(
            !decode_video_source_preflight(&response, &request)
                .unwrap()
                .is_ready()
        );
    }
}
