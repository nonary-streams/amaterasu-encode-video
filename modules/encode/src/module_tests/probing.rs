use super::probe_support::*;
use super::*;

#[test]
fn probe_releases_session_lock_and_preserves_registered_native_table_and_raw_configuration() {
    let (host, mut module) = Host::start();
    let (observations, software) = AcceptanceFactory::install(&module, SoftwareMode::Controlled);
    let mut config = configuration();
    config.sw_preset = Some(String::new());
    config.sw_tune = Some("  film,zerolatency  ".into());
    let reply = host.request(&mut module, probe_request(&software, &config));
    assert_probe_reply(&reply, &software, &config, true);
    assert_no_session(&module);

    let observations = observations.lock().unwrap();
    assert_eq!(observations.attempts.len(), 1);
    let attempt = &observations.attempts[0];
    assert!(
        attempt.shared_lock_available,
        "probe blocked live native sessions"
    );
    assert_eq!(json!(attempt.identity), json!(software));
    assert_eq!(json!(attempt.configuration), json!(config));
    assert_eq!(observations.submitted, 1);
    assert_eq!(observations.drained, 1);
    assert_eq!(observations.closed, 1);
    assert_eq!(observations.packets.len(), 1);
    assert!(observations.packets[0].keyframe);
    assert_eq!(observations.packets[0].timestamp, 1_000_000);
    assert!(observations.packets[0].bytes.starts_with(&[0, 0, 0, 1]));
}

#[test]
fn explicit_fault_hardware_open_does_not_substitute_or_allocate_a_session() {
    let (host, mut module) = Host::start();
    let (observations, _) = AcceptanceFactory::install(&module, SoftwareMode::Controlled);
    let hardware = hardware_identity();
    let config = configuration();
    let reply = host.request(&mut module, open_request(&hardware, &config));
    assert_eq!(reply, json!({"ok":false,"code":"encoder_open_failed"}));
    assert_no_session(&module);
    let observations = observations.lock().unwrap();
    assert_eq!(observations.attempts.len(), 1);
    assert_eq!(json!(observations.attempts[0].identity), json!(hardware));
    assert_eq!(observations.submitted, 0);
    assert_eq!(observations.closed, 0);
}

#[test]
fn transient_probes_leave_the_full_session_operation_history_available() {
    let (host, mut module) = Host::start();
    let (observations, software) = AcceptanceFactory::install(&module, SoftwareMode::Controlled);
    let config = configuration();
    for _ in 0..129 {
        let reply = host.request(&mut module, probe_request(&software, &config));
        assert_probe_reply(&reply, &software, &config, true);
    }
    assert_no_session(&module);
    let reply = host.request(&mut module, open_request(&software, &config));
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["session_id"], 1);
    assert_eq!(module.shared.lock().unwrap().latest_id(), 1);
    assert_eq!(observations.lock().unwrap().closed, 129);
    let closed = host.request(&mut module, json!({"action":"close","session_id":1}));
    assert_eq!(closed["ok"], true);
    assert_eq!(observations.lock().unwrap().closed, 130);
}
