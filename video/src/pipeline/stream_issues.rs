//! Stream failures and fallbacks this service reports first-hand.
//!
//! A stream remembers the operation ID it was opened with (stream-session
//! names it `ss-<id>-video`) and binds its own `video-<epoch>-<id>` operation
//! to it, so capture and encoder providers called under derived IDs report
//! into the same stream. Failure paths queue issues on the stream; the tick
//! publishes them on the SDK's stream-report topic.
use super::{Pipeline, Stage, Stream, stage_name};
use crate::rpc::Send;
use amaterasu_module_sdk::stream_lifecycle::StreamFailure;
use amaterasu_module_sdk::stream_lifecycle::{
    LifecycleLayer, PROVIDER_FALLBACK_CONFIRMED, StreamBinding, StreamIssue, StreamIssueOutbox,
    StreamReport,
};

/// Per-stream reporting state.
#[derive(Debug)]
pub(super) struct StreamIssues {
    /// The operation ID the opening request carried, or this service's own
    /// stream operation when the caller named none.
    operation: String,
    outbox: StreamIssueOutbox,
    ending_reported: bool,
    provider_code: Option<String>,
}

impl StreamIssues {
    /// Bind `stream_operation` to the caller's operation so providers called
    /// under IDs derived from it report into the caller's stream.
    pub(super) fn new(caller_operation: Option<String>, stream_operation: &str) -> Self {
        let mut outbox = StreamIssueOutbox::default();
        let operation = match caller_operation {
            Some(caller_operation) => {
                outbox.push_binding(StreamBinding::new(
                    caller_operation.clone(),
                    stream_operation,
                ));
                caller_operation
            }
            None => stream_operation.to_owned(),
        };
        Self {
            operation,
            outbox,
            ending_reported: false,
            provider_code: None,
        }
    }

    /// Drop a provider code noted for a step that did not end the stream.
    pub(super) fn forget_provider_code(&mut self) {
        self.provider_code = None;
    }

    fn push(&mut self, issue: StreamIssue) {
        if issue.valid() {
            self.outbox.push(issue);
        } else {
            eprintln!(
                "amaterasu video stream issue invalid phase={} code={}",
                issue.phase, issue.code
            );
        }
    }

    fn take_into(&mut self, retired: &mut StreamIssueOutbox) {
        for report in self.outbox.drain() {
            match report {
                StreamReport::Issue(issue) => retired.push(issue),
                StreamReport::Binding(binding) => retired.push_binding(binding),
            }
        }
    }
}

/// A provider code kept as a reason only when it is a short identifier.
fn reason_label(reason: &str) -> Option<&str> {
    let identifier = !reason.is_empty()
        && reason.len() <= 64
        && reason
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'));
    identifier.then_some(reason)
}

fn with_reason(issue: StreamIssue, reason: Option<&str>) -> StreamIssue {
    match reason.and_then(reason_label) {
        Some(reason) => issue.because(reason),
        None => issue,
    }
}

fn capture_stage(stage: Stage) -> bool {
    matches!(
        stage,
        Stage::X11Enumerate
            | Stage::KmsEnumerate
            | Stage::Capture
            | Stage::FallbackEnumerate
            | Stage::ReplaceCapture
            | Stage::FindCapture
            | Stage::CloseCapture
            | Stage::StopCapture
    )
}

fn encoder_stage(stage: Stage) -> bool {
    matches!(
        stage,
        Stage::SelectEncoder
            | Stage::InspectEncoder
            | Stage::ProbeEncoder
            | Stage::Encoder
            | Stage::RestartCloseEncoder
            | Stage::FindEncoder
            | Stage::CloseEncoder
    )
}

/// The dependency whose failure `code` reports: its own prefix first, then
/// the provider the stage was talking to. `None` is this service's own
/// failure. Video reports only under its own layer; the dependency reports
/// its failure itself.
fn dependency_for(stage: Stage, code: &str) -> Option<LifecycleLayer> {
    if code.starts_with("capture") {
        Some(LifecycleLayer::Capture)
    } else if code.starts_with("encoder") {
        Some(LifecycleLayer::EncodeVideo)
    } else if capture_stage(stage) {
        Some(LifecycleLayer::Capture)
    } else if encoder_stage(stage) {
        Some(LifecycleLayer::EncodeVideo)
    } else {
        None
    }
}

fn with_cause(issue: StreamIssue, dependency: Option<LifecycleLayer>) -> StreamIssue {
    match dependency {
        Some(dependency) => issue.caused_by(dependency),
        None => issue,
    }
}

impl Stream {
    /// Keep the provider's own short code as the reason for the next ending
    /// report, where the cleanup code is this service's coarser summary.
    pub(super) fn note_provider_code(&mut self, code: &str) {
        self.issues.provider_code = reason_label(code).map(str::to_owned);
    }

    /// Attribute the original typed report without changing its disposition.
    #[track_caller]
    pub(super) fn failure_issue(&mut self, failure: StreamFailure) -> StreamIssue {
        let code = failure.code();
        let mut issue = failure.at(
            self.issues.operation.clone(),
            LifecycleLayer::EncodeVideo,
            stage_name(self.stage),
        );
        let reason = self.issues.provider_code.take().or_else(|| {
            (code == "encoder_unavailable")
                .then(|| self.encoder_probe_reason.clone())
                .flatten()
        });
        issue = with_cause(issue, dependency_for(self.stage, code));
        with_reason(issue, reason.as_deref())
    }

    pub(super) fn record_terminal_issue(&mut self, issue: StreamIssue) {
        if !self.issues.ending_reported {
            self.issues.ending_reported = true;
            self.issues.push(issue);
        }
    }

    pub(super) fn record_nonterminal_issue(&mut self, issue: StreamIssue) {
        self.issues.push(issue);
    }

    /// Report a resource the stream could not release after retries.
    #[track_caller]
    pub(super) fn report_cleanup_failure(&mut self) {
        let issue = StreamIssue::advisory(
            self.issues.operation.clone(),
            LifecycleLayer::EncodeVideo,
            stage_name(self.stage),
            "cleanup_failed",
        );
        let issue = with_cause(issue, dependency_for(self.stage, ""));
        self.issues.push(issue);
    }

    /// Report a `dependency` failure the stream survived by switching to
    /// `fallback`. `reason` is the dependency's own code.
    #[track_caller]
    pub(super) fn report_confirmed_fallback(&mut self, pending: super::PendingFallback) {
        let issue = if let Some(operation) = pending.operation {
            StreamIssue::demoted(
                operation,
                LifecycleLayer::EncodeVideo,
                PROVIDER_FALLBACK_CONFIRMED,
                pending.code,
                pending.fallback,
            )
            .caused_by(pending.dependency)
        } else {
            // A skipped capability has no provider attempt to retire.
            StreamIssue::demoted(
                self.issues.operation.clone(),
                LifecycleLayer::EncodeVideo,
                stage_name(self.stage),
                pending.code,
                pending.fallback,
            )
        };
        self.issues
            .push(with_reason(issue, pending.reason.as_deref()));
    }
}

impl Pipeline {
    /// Report on the stream-report topic. Off by default so a pipeline in a
    /// test sends only the commands that test scripts.
    pub fn enable_stream_issue_reports(&mut self) {
        self.report_stream_issues = true;
    }

    /// Keep a removed stream's unpublished reports for the next flush.
    pub(super) fn retire_stream_issues(&mut self, stream: &mut Stream) {
        stream.issues.take_into(&mut self.retired_issues);
    }

    /// Publish what each stream recorded. A refused publication keeps the
    /// rest for the next tick; the outboxes are bounded either way.
    pub(super) fn publish_stream_issues(&mut self, send: &mut Send<'_>) {
        let enabled = self.report_stream_issues;
        let outboxes = std::iter::once(&mut self.retired_issues).chain(
            self.streams
                .values_mut()
                .map(|stream| &mut stream.issues.outbox),
        );
        for outbox in outboxes {
            if !enabled {
                outbox.drain().for_each(drop);
                continue;
            }
            if outbox.is_empty() {
                continue;
            }
            if let Err(error) = outbox.flush_with(&mut |command| send(command)) {
                eprintln!("amaterasu video stream issue report failed error={error}");
            }
        }
    }
}
