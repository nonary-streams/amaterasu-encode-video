use super::probe_support::*;
use super::*;

const STREAM_REPORT_TOPIC: &str = "amaterasu.stream.report/1";

fn stream_reports(host: &Host) -> Vec<Value> {
    host.published()
        .into_iter()
        .filter(|(topic, _)| topic == STREAM_REPORT_TOPIC)
        .map(|(_, payload)| payload)
        .collect()
}

#[test]
fn a_failed_open_is_reported_under_the_operation_that_opened_it() {
    let (host, mut module) = Host::start();
    AcceptanceFactory::install(&module, SoftwareMode::Controlled);
    let reply = host.request(
        &mut module,
        open_request(&hardware_identity(), &configuration()),
    );
    assert_eq!(reply["code"], "encoder_open_failed");
    module.tick().unwrap();

    let reports = stream_reports(&host);
    assert_eq!(reports.len(), 1, "{reports:?}");
    let report = &reports[0];
    assert_eq!(report["kind"], "issue");
    assert_eq!(report["operation"], "winner-open");
    assert_eq!(report["layer"], "encode_video");
    assert_eq!(report["phase"], "hardware_mft.open");
    assert_eq!(report["code"], "encoder_open_failed");
    assert_eq!(report["disposition"], "critical_failure");
    assert!(report.get("caused_by").is_none());
    let site = report["site"].as_str().unwrap();
    assert!(site.contains("preparation.rs:"), "{site}");
}

#[test]
fn nothing_is_published_while_reports_are_off() {
    let (host, mut module) = Host::start();
    module.report_stream_issues = false;
    AcceptanceFactory::install(&module, SoftwareMode::Controlled);
    host.request(
        &mut module,
        open_request(&hardware_identity(), &configuration()),
    );
    module.tick().unwrap();
    assert!(stream_reports(&host).is_empty());
    assert!(module.stream_issues.is_empty(), "discarded, not kept");
}
