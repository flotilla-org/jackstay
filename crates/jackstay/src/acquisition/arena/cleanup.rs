use std::{
    collections::BTreeMap,
    fmt::Debug,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering::SeqCst},
    },
    thread::JoinHandle,
};

use serde::{Deserialize, Serialize};

use super::{
    ACTIVE, AdmissionError, ArenaError, ArenaProducer, CAPACITY_EPOCH, CLAIM_SLOT_LEN, ClaimMap, FrameLease, HEADER_LEN, IncarnationId,
    QUIESCENT, RELEASE_ID, RELEASE_PENDING, RELEASE_VALUE, process::ProcessWatch, wait,
};

/// Producer-owned access to a registered native completion timeline. Returning a
/// value asserts that the corresponding consumer GPU work has completed, not
/// merely been submitted. The imported handle must remain alive through draining.
pub trait ReleaseTimeline: Send + Sync + Debug {
    /// Read the monotonic completion value without blocking.
    fn completed_value(&self) -> crate::Result<u64>;

    /// Arrange a notification when this value is complete, including when it
    /// completed before registration. Must not block. A notification only wakes
    /// the owner, which checks completed_value again before returning credit.
    /// An error must mean that no notification was registered.
    fn notify_at(&self, value: u64, notification: ReleaseNotification) -> crate::Result<()>;
}

/// Setup-channel registration result, bound to one claim mapping as well as its
/// incarnation. The random scope prevents matching local IDs in another arena or
/// a restarted producer from satisfying this registration accidentally.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseTimelineRegistration {
    incarnation: u64,
    scope: [u8; 16],
    id: u64,
}

/// A one-shot wake owned by a bounded release observation. Backends may call
/// notify inline or from a completion callback. It carries no frame ownership.
#[derive(Debug, Clone)]
pub struct ReleaseNotification(Arc<NotificationInner>);

#[derive(Debug)]
struct NotificationInner {
    fired: AtomicBool,
    wake: Arc<wait::Wake>,
}

impl ReleaseNotification {
    pub fn notify(&self) -> std::io::Result<()> {
        if !self.0.fired.swap(true, SeqCst) {
            self.0.wake.signal()?;
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
pub(super) struct CleanupRegistry {
    observers: BTreeMap<IncarnationId, IncarnationObserver>,
}

impl CleanupRegistry {
    pub(super) fn track_process(
        &mut self,
        claims: Arc<ClaimMap>,
        native_resources: bool,
        process: Arc<ProcessWatch>,
    ) -> Result<(), ArenaError> {
        let incarnation = claims.incarnation;
        let observer = IncarnationObserver::new(claims, native_resources, Some(process))?;
        assert!(self.observers.insert(incarnation, observer).is_none(), "new incarnation");
        Ok(())
    }

    pub(super) fn remove(&mut self, incarnation: IncarnationId) {
        self.observers.remove(&incarnation);
    }
}

#[derive(Debug)]
struct ObservationState {
    claims: Arc<ClaimMap>,
    native_resources: bool,
    process: Option<Arc<ProcessWatch>>,
    unresolved_native_claims: usize,
    timelines: Vec<Arc<dyn ReleaseTimeline>>,
    armed: Vec<Option<ReleaseNotification>>,
    failure: Option<String>,
    notification_failed: bool,
    released: usize,
}

impl ObservationState {
    fn fail(&mut self, reason: String) {
        self.claims.close();
        self.failure = Some(reason);
    }

    fn fail_notification(&mut self, reason: String) {
        self.notification_failed = true;
        self.fail(reason);
    }

    fn observe_process(&mut self) {
        let Some(process) = &self.process else {
            return;
        };
        match process.exited() {
            Ok(false) => return,
            Ok(true) => {}
            Err(error) => {
                self.process = None;
                self.fail_notification(error.to_string());
                return;
            }
        }
        self.process = None;
        self.claims.close();
        // The watch was bound before this remote grant escaped. The OS event
        // proves no admitted process thread can still read or publish a claim.
        self.claims.word(QUIESCENT).store(1, SeqCst);
        for index in 0..self.claims.frames {
            if self.claims.slot(index).load(SeqCst) == 0 {
                continue;
            }
            match self.claims.release_word(index, RELEASE_PENDING).load(SeqCst) {
                // Submitted deferred uses retain their imported completion
                // source, including on a CPU arena used for asynchronous work.
                1 => {}
                0 if !self.native_resources && self.timelines.is_empty() => {
                    self.claims.return_credit(index);
                    self.released = self.released.saturating_add(1);
                }
                0 => self.unresolved_native_claims += 1,
                _ => self.fail("invalid release state at process exit".to_owned()),
            }
        }
    }

    fn failure_reason(&self) -> Option<String> {
        self.failure.clone().or_else(|| {
            (self.unresolved_native_claims != 0).then(|| {
                format!(
                    "process exited with {} asynchronous claims lacking completion evidence; resources remain quarantined",
                    self.unresolved_native_claims,
                )
            })
        })
    }

    fn poll(&mut self) {
        self.observe_process();
        if self.failure.is_some() {
            return;
        }
        for index in 0..self.claims.frames {
            // A completed value can be observed before its queued callback runs.
            // Keep that callback charged to this slot until it fires, even if the
            // slot is already reused, so queued native callbacks remain bounded.
            if self.armed[index]
                .as_ref()
                .is_some_and(|notification| notification.0.fired.load(SeqCst))
            {
                self.armed[index] = None;
            }
            match self.claims.release_word(index, RELEASE_PENDING).load(SeqCst) {
                0 => continue,
                1 => {}
                _ => {
                    self.fail("invalid deferred release state".to_owned());
                    break;
                }
            }
            let id = self.claims.release_word(index, RELEASE_ID).load(SeqCst);
            let value = self.claims.release_word(index, RELEASE_VALUE).load(SeqCst);
            let Some(timeline) = id.checked_sub(1).and_then(|id| self.timelines.get(id as usize)) else {
                self.fail("deferred release names an unregistered timeline".to_owned());
                break;
            };
            let completed = match timeline.completed_value() {
                Ok(value) => value,
                Err(error) => {
                    self.fail(error.to_string());
                    break;
                }
            };
            if completed >= value {
                self.claims.return_credit(index);
                self.released = self.released.saturating_add(1);
            } else if self.armed[index].is_none() {
                let notification = ReleaseNotification(Arc::new(NotificationInner {
                    fired: AtomicBool::new(false),
                    wake: Arc::clone(&self.claims.release_wake),
                }));
                match timeline.notify_at(value, notification.clone()) {
                    Ok(()) => self.armed[index] = Some(notification),
                    Err(error) => {
                        self.fail(error.to_string());
                        break;
                    }
                }
            }
        }
    }
}

/// At most one sleeping thread per monitored incarnation. Local CPU-only
/// consumers need none; remote consumers and native timeline registrations use
/// this same owner. Manual refresh and the worker serialize reclamation.
#[derive(Debug)]
struct IncarnationObserver {
    state: Arc<Mutex<ObservationState>>,
    stop: Arc<AtomicBool>,
    wake: Arc<wait::Wake>,
    worker: Option<JoinHandle<()>>,
}

impl IncarnationObserver {
    fn new(claims: Arc<ClaimMap>, native_resources: bool, process: Option<Arc<ProcessWatch>>) -> Result<Self, ArenaError> {
        // Socket directions are independent: consumers read data/capacity wakes
        // on one end and write handoff wakes back to this sole reverse reader.
        let mut receiver = wait::Receiver::from_fd(claims.wake.fd()?)?;
        let wake = Arc::clone(&claims.release_wake);
        let state = Arc::new(Mutex::new(ObservationState {
            armed: vec![None; claims.frames],
            claims,
            native_resources,
            process,
            unresolved_native_claims: 0,
            timelines: Vec::new(),
            failure: None,
            notification_failed: false,
            released: 0,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_state = Arc::clone(&state);
        let worker_stop = Arc::clone(&stop);
        let worker = std::thread::Builder::new().name("jackstay-cleanup".to_owned()).spawn(move || {
            while !worker_stop.load(SeqCst) {
                // Drain BEFORE checking shared state. A subsequent handoff or
                // completed-event callback leaves a byte for the following poll.
                if let Err(error) = receiver.drain() {
                    worker_state
                        .lock()
                        .expect("incarnation cleanup state")
                        .fail_notification(error.to_string());
                    break;
                }
                worker_state.lock().expect("incarnation cleanup state").poll();
                if worker_stop.load(SeqCst) {
                    break;
                }
                // Retain the watch FD while sleeping: a concurrent host refresh
                // can consume its exit event and remove it from shared state.
                let process = worker_state.lock().expect("incarnation cleanup state").process.clone();
                if let Err(error) = receiver.sleep(process.as_ref().map(|process| process.fd())) {
                    worker_state
                        .lock()
                        .expect("incarnation cleanup state")
                        .fail_notification(error.to_string());
                    break;
                }
            }
        })?;
        Ok(Self {
            state,
            stop,
            wake,
            worker: Some(worker),
        })
    }
}

impl Drop for IncarnationObserver {
    fn drop(&mut self) {
        self.stop.store(true, SeqCst);
        let _ = self.wake.signal();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl ClaimMap {
    fn release_word(&self, index: usize, field: usize) -> &std::sync::atomic::AtomicU64 {
        assert!(index < self.frames && matches!(field, RELEASE_ID | RELEASE_VALUE | RELEASE_PENDING));
        self.word(HEADER_LEN + index * CLAIM_SLOT_LEN + field)
    }

    fn registered_timeline(&self, index: usize) -> &std::sync::atomic::AtomicU64 {
        assert!(index < self.frames);
        self.word(HEADER_LEN + self.frames * CLAIM_SLOT_LEN + index * 8)
    }

    pub(super) fn return_credit(&self, index: usize) {
        // Reset handoff state before publishing an empty slot: a new acquirer
        // may reuse it immediately and must never inherit an old pending release.
        self.release_word(index, RELEASE_PENDING).store(0, SeqCst);
        self.slot(index).store(0, SeqCst);
        if self.word(CAPACITY_EPOCH).fetch_add(1, SeqCst) == u64::MAX {
            self.close();
        }
        let _ = self.signal(wait::CAPACITY);
    }
}

impl ArenaProducer {
    /// Persistent per-incarnation recovery failures. Healthy incarnations keep
    /// their acquisition and completion guarantees while these claims drain.
    pub fn cleanup_failures(&self) -> Vec<CleanupFailure> {
        self.cleanup
            .observers
            .iter()
            .filter_map(|(incarnation, observer)| {
                observer
                    .state
                    .lock()
                    .expect("incarnation cleanup state")
                    .failure_reason()
                    .map(|reason| CleanupFailure {
                        incarnation: *incarnation,
                        reason,
                    })
            })
            .collect()
    }

    /// Retry observing a retained completion source after the host repairs its
    /// backend. This neither reopens acquisition nor force-releases any claim.
    pub fn retry_cleanup(&mut self, incarnation: IncarnationId) -> Result<(), ArenaError> {
        if !self.claims.contains_key(&incarnation) {
            return Err(AdmissionError::UnknownIncarnation.into());
        }
        if let Some(observer) = self.cleanup.observers.get(&incarnation) {
            let mut state = observer.state.lock().expect("incarnation cleanup state");
            if state.notification_failed {
                // A backend retry cannot repair a dead notification channel or
                // restart its reader. Keep this recovery failure visible.
                return Err(std::io::Error::other("release observation channel failed; claims remain quarantined").into());
            }
            state.failure = None;
            observer.wake.signal()?;
            if let Some(reason) = state.failure_reason() {
                return Err(ArenaError::RecoveryRequired { reason });
            }
        }
        Ok(())
    }

    /// Import each consumer release timeline once through setup, before starting
    /// asynchronous use. Registration opts CPU resources into conservative
    /// asynchronous crash handling as well: exit cannot prove GPU completion. Registrations
    /// and outstanding backend notifications are bounded by the incarnation's
    /// holding reservation. The observer also runs while publication is idle.
    pub fn register_release_timeline(
        &mut self,
        incarnation: IncarnationId,
        timeline: Arc<dyn ReleaseTimeline>,
    ) -> Result<ReleaseTimelineRegistration, ArenaError> {
        let claims = self.claims.get(&incarnation).ok_or(AdmissionError::UnknownIncarnation)?;
        if claims.word(ACTIVE).load(SeqCst) == 0 {
            return Err(ArenaError::Closed);
        }
        if let std::collections::btree_map::Entry::Vacant(entry) = self.cleanup.observers.entry(incarnation) {
            entry.insert(IncarnationObserver::new(Arc::clone(claims), self.native_resources, None)?);
        }
        let observer = &self.cleanup.observers[&incarnation];
        let mut state = observer.state.lock().expect("incarnation cleanup state");
        if claims.word(ACTIVE).load(SeqCst) == 0 {
            return Err(ArenaError::Closed);
        }
        if state.timelines.len() >= claims.frames {
            return Err(ArenaError::Configuration(
                "release timeline capacity equals the holding reservation",
            ));
        }
        let index = state.timelines.len();
        let id = index as u64 + 1;
        state.timelines.push(timeline);
        claims.registered_timeline(index).store(id, SeqCst);
        Ok(ReleaseTimelineRegistration {
            incarnation: incarnation.0,
            scope: claims.scope,
            id,
        })
    }

    /// Refresh completions and return credit released since the previous call,
    /// including automatic observation while idle. Publication and admission
    /// call this too; hosts need not poll to wake a consumer's capacity wait.
    /// Consumer shutdown never clears a pending claim.
    pub fn poll_cleanup(&mut self) -> Result<usize, ArenaError> {
        let mut released = 0_usize;
        for observer in self.cleanup.observers.values() {
            let mut state = observer.state.lock().expect("incarnation cleanup state");
            state.poll();
            released = released.saturating_add(std::mem::take(&mut state.released));
        }
        self.collect_quiescent();
        Ok(released)
    }
}

#[derive(Debug, Clone)]
pub struct CleanupFailure {
    pub incarnation: IncarnationId,
    pub reason: String,
}

/// Failed handoff retains the caller's lease. In particular, a C wrapper must
/// keep this frame in its lease table when reporting the error to its caller.
#[derive(Debug)]
pub struct RejectedDeferredRelease {
    pub error: ArenaError,
    pub frame: Box<FrameLease>,
}

impl FrameLease {
    /// Hand ownership of this claim to the producer's completion observer. The
    /// returned frame on failure remains leased; success consumes it so it can
    /// no longer expose a byte slice after the completion observer releases it.
    pub fn defer_release(mut self, registration: &ReleaseTimelineRegistration, value: u64) -> Result<(), RejectedDeferredRelease> {
        let claims = &self.claim.owner.claims;
        if registration.incarnation != claims.incarnation.0
            || registration.scope != claims.scope
            || !(0..claims.frames).any(|index| claims.registered_timeline(index).load(SeqCst) == registration.id)
            || registration.id == 0
        {
            return Err(RejectedDeferredRelease {
                error: ArenaError::Mapping("release timeline belongs to another incarnation or is unregistered"),
                frame: Box::new(self),
            });
        }
        // Disable RAII release BEFORE making the handoff visible. The producer
        // can observe an already-complete event and clear/reuse this claim slot
        // immediately after the final store. Nothing below may touch it again.
        self.claim.release_on_drop = false;
        claims.release_word(self.claim.slot, RELEASE_ID).store(registration.id, SeqCst);
        claims.release_word(self.claim.slot, RELEASE_VALUE).store(value, SeqCst);
        claims.release_word(self.claim.slot, RELEASE_PENDING).store(1, SeqCst);
        // Ownership is already transferred: a wake failure must not return the
        // frame or clear its claim. Close acquisition; the owner retains it for
        // observation/recovery. Normal socket saturation means a wake is pending.
        if claims.release_wake.signal().is_err() {
            claims.close();
        }
        Ok(())
    }
}
