use super::*;
use crate::model::{EncoderDescription, Packet};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Observations {
    submitted: usize,
    drained: usize,
    closed: usize,
}

struct TestFactory {
    packets: Mutex<VecDeque<Packet>>,
    observations: Arc<Mutex<Observations>>,
    failure: Option<&'static str>,
}

struct TestBackend {
    packets: VecDeque<Packet>,
    observations: Arc<Mutex<Observations>>,
    failure: Option<&'static str>,
}

impl Factory for TestFactory {
    fn enumerate(&self) -> Result<Vec<EncoderDescription>, &'static str> {
        Ok(Vec::new())
    }
    fn open(&self, _: EncoderIdentity, _: Configuration) -> Result<Box<dyn Backend>, &'static str> {
        Ok(Box::new(TestBackend {
            packets: std::mem::take(&mut *self.packets.lock().unwrap()),
            observations: self.observations.clone(),
            failure: self.failure,
        }))
    }
}

impl Backend for TestBackend {
    fn submit(
        &mut self,
        bytes: Vec<u8>,
        timestamp: i64,
        force: bool,
    ) -> Result<SubmitResult, &'static str> {
        assert_eq!(bytes.len(), 16 * 16 * 3 / 2);
        assert!(bytes[..256].iter().all(|value| *value == 16));
        assert!(bytes[256..].iter().all(|value| *value == 128));
        assert_eq!(timestamp, PROBE_TIMESTAMP);
        assert!(force);
        self.observations.lock().unwrap().submitted += 1;
        Ok(SubmitResult::Accepted)
    }
    fn receive(&mut self) -> Result<ReceiveResult, &'static str> {
        Ok(ReceiveResult::Ready(self.packets.pop_front()))
    }
    fn drain(&mut self) -> Result<bool, &'static str> {
        self.observations.lock().unwrap().drained += 1;
        if self.failure == Some("drain") {
            Err("encoder_drain_failed")
        } else {
            Ok(true)
        }
    }
    fn close(self: Box<Self>) -> Result<(), &'static str> {
        self.observations.lock().unwrap().closed += 1;
        if self.failure == Some("close") {
            Err("encoder_shutdown_failed")
        } else {
            Ok(())
        }
    }
}

fn identity() -> EncoderIdentity {
    EncoderIdentity {
        clsid: "test-probe".into(),
        hardware_url: None,
        friendly_name: "Test probe".into(),
    }
}

fn configuration() -> Configuration {
    serde_json::from_value(json!({"width":16,"height":16,"fps":30,"bitrate":1000000})).unwrap()
}

fn idr() -> Packet {
    Packet {
        bytes: vec![
            0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 1, 0, 0, 0, 1, 0x65, 1,
        ],
        timestamp_100ns: PROBE_TIMESTAMP,
        keyframe: true,
    }
}

fn factory(packet: Option<Packet>, failure: Option<&'static str>) -> TestFactory {
    TestFactory {
        packets: Mutex::new(packet.into_iter().collect()),
        observations: Arc::default(),
        failure,
    }
}

#[test]
fn settled_probe_requires_actual_annex_b_idr_timestamp_and_full_teardown() {
    let factory = factory(Some(idr()), None);
    let config = configuration();
    let reply = run(&factory, identity(), config.clone()).unwrap();
    assert_eq!(reply["usable"], true);
    assert_eq!(reply["settled"], true);
    assert_eq!(reply["configuration"], json!(config));
    let observations = factory.observations.lock().unwrap();
    assert_eq!(
        (
            observations.submitted,
            observations.drained,
            observations.closed
        ),
        (1, 1, 1)
    );
}

#[test]
fn flag_only_avcc_wrong_timestamp_empty_and_failed_drain_are_unusable_after_close() {
    let mut wrong_timestamp = idr();
    wrong_timestamp.timestamp_100ns += 1;
    let mut flag_only = idr();
    flag_only.bytes = vec![0, 0, 0, 1, 0x41, 1];
    let avcc = Packet {
        bytes: vec![0, 0, 0, 2, 0x65, 1],
        ..idr()
    };
    for (packet, failure) in [
        (Some(wrong_timestamp), None),
        (Some(flag_only), None),
        (Some(avcc), None),
        (None, None),
        (Some(idr()), Some("drain")),
    ] {
        let factory = factory(packet, failure);
        let reply = run(&factory, identity(), configuration()).unwrap();
        assert_eq!(reply["usable"], false);
        assert_eq!(reply["settled"], true);
        assert_eq!(reply["code"], "encoder_unusable");
        assert_eq!(factory.observations.lock().unwrap().closed, 1);
    }
}

#[test]
fn failed_cleanup_never_reports_a_safely_unusable_candidate() {
    let factory = factory(Some(idr()), Some("close"));
    assert_eq!(
        run(&factory, identity(), configuration()),
        Err("encoder_probe_cleanup_failed")
    );
    assert_eq!(factory.observations.lock().unwrap().closed, 1);
}

#[cfg(target_os = "linux")]
#[test]
fn negotiated_hevc_and_av1_probes_validate_their_own_bitstreams() {
    let mut hevc = Vec::new();
    for unit in [
        &[0x40, 0x01, 0x0c][..],
        &[0x42, 0x01, 0x01],
        &[0x44, 0x01, 0xc1],
        &[0x26, 0x01, 0xaf],
    ] {
        hevc.extend_from_slice(&[0, 0, 0, 1]);
        hevc.extend_from_slice(unit);
    }
    let av1 = vec![0x12, 0x00, 0x0a, 0x01, 0x00, 0x32, 0x01, 0x00];
    for (codec, bytes) in [("hevc", hevc), ("av1", av1)] {
        let config = serde_json::from_value(json!({
            "width":16,"height":16,"fps":30,"bitrate":1000000,
            "video_stream_format":{"codec":codec,"bit_depth":8,"dynamic_range":"sdr"}
        }))
        .unwrap();
        let factory = factory(
            Some(Packet {
                bytes,
                timestamp_100ns: PROBE_TIMESTAMP,
                keyframe: true,
            }),
            None,
        );
        let reply = run(&factory, identity(), config).unwrap();
        assert_eq!(reply["usable"], true, "{codec}: {reply}");
        assert_eq!(reply["codec"], codec);
        assert_eq!(factory.observations.lock().unwrap().closed, 1);
    }
}
