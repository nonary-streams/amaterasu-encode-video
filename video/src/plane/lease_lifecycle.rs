//! Retirement barriers for native stages that can retain source GPU leases.
use amaterasu_media::EncoderApiV1_2;
use std::{collections::BTreeMap, ffi::c_void};

type Cancel = unsafe extern "C" fn(*mut c_void, u64);

struct Stage {
    context: *mut c_void,
    cancel: Cancel,
    stream: u64,
}

impl Stage {
    fn finish(&self) {
        // SAFETY: registration retains all native bindings until these mandatory
        // callbacks return. The ABI requires synchronous retirement, even when
        // native work fails or the control stream was already closed.
        unsafe { (self.cancel)(self.context, self.stream) };
    }
}

struct Stages {
    encoder: Stage,
}

#[derive(Default)]
pub(super) struct LeasedStreams {
    // Only active video streams are tracked. Final control cleanup removes an
    // entry; changing an encoder retires the previous one before replacing it.
    values: BTreeMap<u64, Stages>,
}

impl LeasedStreams {
    /// # Safety
    /// Both native bindings must remain live until finish/finish_all completes.
    /// Tables and callbacks must obey their advertised lease cancellation ABI.
    pub unsafe fn register(&mut self, video: u64, encoder: u64, api: &EncoderApiV1_2) {
        if let Some(previous) = self.values.get(&video) {
            if previous.encoder.stream == encoder {
                return;
            }
            previous.encoder.finish();
        }
        self.values.insert(
            video,
            Stages {
                encoder: Stage {
                    context: api.base.context,
                    cancel: api.cancel_dmabuf_leases,
                    stream: encoder,
                },
            },
        );
    }

    pub fn finish(&mut self, video: u64) {
        if let Some(stages) = self.values.remove(&video) {
            stages.encoder.finish();
        }
    }

    pub fn finish_all(&mut self) {
        let values = std::mem::take(&mut self.values);
        // A processor may own output retained by multiple encoders. Retire all
        // downstream users before retiring any processor-owned GPU work.
        for stages in values.values() {
            stages.encoder.finish();
        }
    }
}
