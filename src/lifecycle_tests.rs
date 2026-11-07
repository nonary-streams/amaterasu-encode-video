use super::*;
use amaterasu_module_sdk::abi::*;
use serde_json::{Value, json};
use std::ffi::c_void;

#[derive(Default)]
struct Host {
    completed: Vec<(u64, Value)>,
    commands: Vec<Command>,
}

unsafe extern "C" fn invoke(
    context: *mut c_void,
    input: BorrowedBytes,
    output: ReplySink,
) -> Status {
    // SAFETY: each fixture keeps its endpoint and buffers alive through the call.
    let host = unsafe { &mut *context.cast::<Host>() };
    let command: Command = serde_json::from_slice(unsafe { input.read().unwrap() }).unwrap();
    host.commands.push(command.clone());
    let response = match command {
        Command::GetSettings {} => Response::Result {
            payload: json!({"enabled":false}),
        },
        Command::Poll {} => Response::Pending,
        Command::Complete { request, payload } => {
            host.completed.push((request, payload));
            Response::Ok
        }
        _ => Response::Ok,
    };
    // SAFETY: the host supplied this callback-lifetime reply sink.
    unsafe { output.send(&serde_json::to_vec(&response).unwrap()) };
    STATUS_OK
}

fn module(host: &mut Host) -> Video {
    let endpoint = HostApi {
        header: AbiHeader::of::<HostApi>(),
        context: (host as *mut Host).cast(),
        invoke,
    };
    // SAFETY: every test retains host until after module destruction.
    let mut module = Video::start(unsafe { RuntimeClient::from_endpoint(endpoint) }).unwrap();
    let settings = Settings {
        enabled: true,
        capture_providers: [model::CaptureProvider::Dxgi].into(),
        ..Settings::default()
    };
    module.pipeline = Pipeline::new(
        settings,
        "shutdown-test".into(),
        module.pipeline.queues.clone(),
    );
    module
}

fn open() -> Operation {
    serde_json::from_value(json!({
        "action":"open", "capture_provider":"dxgi", "output":"fixture-output",
        "encoder":0,
        "configuration":{"width":1920,"height":1080,"fps":60,"bitrate":5000000}
    }))
    .unwrap()
}

#[test]
fn quiesce_waits_for_an_owned_open_to_finish_cleanup_before_reporting_ready() {
    let mut host = Host::default();
    let mut module = module(&mut host);
    module
        .pipeline
        .handle("consumer", 91, open(), &mut |command| {
            module.runtime.send_ephemeral(command)
        });
    assert!(!module.quiesce().unwrap());
    module.tick().unwrap();
    assert!(module.quiesce().unwrap());
    assert!(
        host.completed
            .iter()
            .any(|(request, payload)| { *request == 91 && payload["code"] == "video_shutdown" })
    );
    assert!(!host.commands.iter().any(|command| {
        matches!(command, Command::Request { payload, .. } if payload["action"] == "open")
    }));
}

#[test]
fn shutdown_announcement_fences_new_video_open_without_starting_cleanup() {
    let mut host = Host::default();
    let mut module = module(&mut host);
    module
        .pipeline
        .handle("consumer", 91, open(), &mut |command| {
            module.runtime.send_ephemeral(command)
        });
    module.prepare_shutdown(5_000).unwrap();
    assert!(host.completed.is_empty());
    module
        .pipeline
        .handle("consumer", 92, open(), &mut |command| {
            module.runtime.send_ephemeral(command)
        });
    assert!(
        host.completed
            .iter()
            .any(|(request, payload)| { *request == 92 && payload["code"] == "shutting_down" })
    );
    assert!(!host.completed.iter().any(|(request, _)| *request == 91));
}
