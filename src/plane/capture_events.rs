//! Notification callbacks only latch metadata and wake readers. Acquiring frames
//! and coordinating swaps remain on the video module thread.
use super::*;
use amaterasu_module_sdk::capture::{CaptureNotice, CaptureNotifications, SwapDecision};

struct Subscription {
    context: *mut c_void,
    capture: u64,
    unsubscribe: unsafe extern "C" fn(*mut c_void, u64),
    resolve: unsafe extern "C" fn(*mut c_void, u64, u64, u32, BorrowedBytes) -> Status,
    notifications: CaptureNotifications,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        // SAFETY: NativePlane retains every source binding until all subscriptions
        // retire. This callback is a mandatory notification lifetime barrier.
        unsafe { (self.unsubscribe)(self.context, self.capture) };
        self.notifications.close();
    }
}

#[derive(Default)]
pub(super) struct Subscriptions {
    values: BTreeMap<u64, Subscription>,
    work: Option<amaterasu_module_sdk::WorkNotifier>,
}

impl Subscriptions {
    pub fn new(work: amaterasu_module_sdk::WorkNotifier) -> Self {
        Self {
            work: Some(work),
            ..Self::default()
        }
    }

    pub fn notice(
        &mut self,
        video: u64,
        capture: u64,
        binding: &NativeBinding,
    ) -> Result<Option<CaptureNotice>, &'static str> {
        if let std::collections::btree_map::Entry::Vacant(entry) = self.values.entry(video) {
            // SAFETY: NativePlane binds trusted capture services with the legacy prefix.
            let prefix = unsafe { binding.table::<CaptureApiPrefix>() }
                .map_err(|_| "capture_bind_failed")?;
            if !capture_events_available(prefix.base.header) {
                return Ok(None);
            }
            // SAFETY: version and table size cover the new optional tail.
            let api =
                unsafe { binding.table::<CaptureApiV1_7>() }.map_err(|_| "capture_bind_failed")?;
            let notifications = self
                .work
                .as_ref()
                .map_or_else(CaptureNotifications::new, |work| {
                    CaptureNotifications::with_work_notifier(work.clone())
                });
            // SAFETY: on success this reader stays in the subscription until the
            // provider's unsubscribe barrier. Failure must retain no callbacks.
            let result = unsafe {
                (api.subscribe_events)(api.base.base.context, capture, notifications.native_sink())
            };
            match result {
                MEDIA_UNSUPPORTED | MEDIA_BUSY => return Ok(None),
                STATUS_OK => {}
                _ => return Err("capture_event_subscription_failed"),
            }
            entry.insert(Subscription {
                context: api.base.base.context,
                capture,
                unsubscribe: api.unsubscribe_events,
                resolve: api.resolve_swap,
                notifications,
            });
        }
        Ok(Some(self.values[&video].notifications.take()))
    }

    pub fn resolve(
        &mut self,
        video: u64,
        id: u64,
        decision: SwapDecision,
    ) -> Result<(), &'static str> {
        let subscription = self
            .values
            .get(&video)
            .ok_or("capture_events_unavailable")?;
        let (result, reason) = match &decision {
            SwapDecision::Accepted => (CAPTURE_SWAP_ACCEPTED, ""),
            SwapDecision::Rejected { reason } => (CAPTURE_SWAP_REJECTED, reason.as_str()),
        };
        // SAFETY: source binding and reason bytes remain live, on the module
        // thread. No receiver lock is held across provider code.
        let status = unsafe {
            (subscription.resolve)(
                subscription.context,
                subscription.capture,
                id,
                result,
                BorrowedBytes::new(reason.as_bytes()),
            )
        };
        if status != STATUS_OK {
            return Err("capture_swap_reply_failed");
        }
        subscription.notifications.clear_swap(id);
        Ok(())
    }

    pub fn remove(&mut self, video: u64) {
        self.values.remove(&video);
    }
    pub fn clear(&mut self) {
        self.values.clear();
    }
}
