use std::{
    collections::BTreeMap,
    fmt::Debug,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering::SeqCst},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use super::{
    ACTIVE, AdmissionError, ArenaError, ArenaProducer, CAPACITY_EPOCH, CLAIM_SLOT_LEN, ClaimMap, FrameLease, HEADER_LEN, IncarnationId,
    QUIESCENT, RELEASE_ID, RELEASE_PENDING, RELEASE_VALUE, process::ProcessWatch, wait,
};

/// Access to a registered completion timeline. Returning a
/// value asserts that the corresponding consumer GPU work has completed, not
/// merely been submitted. Producer and consumer owners each retain a handle
/// through draining, so either can observe completion after the other shuts down.
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
    pub(super) incarnation: u64,
    pub(super) scope: [u8; 16],
    pub(super) id: u64,
}

/// A one-shot wake owned by a bounded release observation. Backends may call
/// notify inline or from a completion callback. It carries no frame ownership.
#[derive(Debug, Clone)]
pub struct ReleaseNotification(Arc<NotificationInner>);

#[derive(Debug)]
struct NotificationInner {
    fired: AtomicBool,
    wake: NotificationWake,
}

#[derive(Debug)]
enum NotificationWake {
    Producer(Arc<wait::Wake>),
    Consumer(Arc<super::retirement::LocalWake>),
}

impl ReleaseNotification {
    pub(super) fn has_fired(&self) -> bool {
        self.0.fired.load(SeqCst)
    }

    pub(super) fn for_local(wake: Arc<super::retirement::LocalWake>) -> Self {
        Self(Arc::new(NotificationInner {
            fired: AtomicBool::new(false),
            wake: NotificationWake::Consumer(wake),
        }))
    }

    pub fn notify(&self) -> std::io::Result<()> {
        if !self.0.fired.swap(true, SeqCst) {
            match &self.0.wake {
                NotificationWake::Producer(wake) => wake.signal()?,
                NotificationWake::Consumer(wake) => wake.signal(),
            }
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
pub(super) struct CleanupRegistry {
    observers: BTreeMap<IncarnationId, IncarnationObserver>,
}

impl CleanupRegistry {
    pub(super) fn track(
        &mut self,
        claims: Arc<ClaimMap>,
        native_resources: bool,
        process: Option<Arc<ProcessWatch>>,
        drain_timeout: Duration,
    ) -> Result<(), ArenaError> {
        let incarnation = claims.incarnation;
        let observer = IncarnationObserver::new(claims, native_resources, process, drain_timeout)?;
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
    drain_timeout: Duration,
    drain_deadline: Option<Instant>,
    drain_expired: bool,
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
        self.claims.acknowledge_quiescent();
        for index in 0..self.claims.frames + 2 {
            self.claims.mapping_slot(index).store(0, SeqCst);
        }
        for index in 0..self.claims.frames {
            if self.claims.slot(index).load(SeqCst) == 0 {
                continue;
            }
            match self.claims.release_word(index, RELEASE_PENDING).load(SeqCst) {
                // Submitted deferred uses retain their imported completion
                // source, including on a CPU arena used for asynchronous work.
                1 | 2 => {
                    self.claims.release_word(index, RELEASE_PENDING).store(2, SeqCst);
                }
                0 if !self.native_resources && self.timelines.is_empty() => {
                    self.claims.return_credit(index);
                    self.released = self.released.saturating_add(1);
                }
                0 => self.unresolved_native_claims += 1,
                _ => self.fail("invalid release state at process exit".to_owned()),
            }
        }
    }

    fn drained(&self) -> bool {
        self.claims.word(QUIESCENT).load(SeqCst) != 0
            && !self.claims.has_mappings()
            && (0..self.claims.frames).all(|index| self.claims.slot(index).load(SeqCst) == 0)
    }

    fn observe_deadline(&mut self) {
        if self.claims.word(ACTIVE).load(SeqCst) != 0 || self.drained() {
            return;
        }
        let now = Instant::now();
        if self.drain_deadline.is_none() {
            self.drain_deadline = now.checked_add(self.drain_timeout);
            if self.drain_deadline.is_none() {
                self.fail("drain deadline is not representable; resources remain quarantined".to_owned());
            }
        }
        if self.drain_deadline.is_some_and(|deadline| now >= deadline) {
            self.drain_expired = true;
        }
    }

    fn wake_deadline(&self) -> Option<Instant> {
        if self.drained() || self.failure_reason().is_some() {
            None
        } else {
            self.drain_deadline
        }
    }

    fn failure_reason(&self) -> Option<String> {
        if self.drained() {
            return None;
        }
        self.failure
            .clone()
            .or_else(|| {
                (self.unresolved_native_claims != 0).then(|| {
                    format!(
                        "process exited with {} asynchronous claims lacking completion evidence; resources remain quarantined",
                        self.unresolved_native_claims,
                    )
                })
            })
            .or_else(|| {
                self.drain_expired
                    .then(|| "drain deadline expired; claims or claim-page access remain unresolved".to_owned())
            })
    }

    fn poll(&mut self) {
        self.observe_process();
        self.observe_deadline();
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
                1 | 2 => {}
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
            if completed >= value && self.claims.release_word(index, RELEASE_PENDING).load(SeqCst) == 2 {
                self.claims.return_credit(index);
                self.released = self.released.saturating_add(1);
            } else if completed < value && self.armed[index].is_none() {
                let notification = ReleaseNotification(Arc::new(NotificationInner {
                    fired: AtomicBool::new(false),
                    wake: NotificationWake::Producer(Arc::clone(&self.claims.release_wake)),
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

/// One sleeping owner per admitted incarnation handles closure, process exit,
/// and release completion. Manual refresh and the worker serialize reclamation.
#[derive(Debug)]
struct IncarnationObserver {
    state: Arc<Mutex<ObservationState>>,
    stop: Arc<AtomicBool>,
    wake: Arc<wait::Wake>,
    worker: Option<JoinHandle<()>>,
}

impl IncarnationObserver {
    fn new(
        claims: Arc<ClaimMap>,
        native_resources: bool,
        process: Option<Arc<ProcessWatch>>,
        drain_timeout: Duration,
    ) -> Result<Self, ArenaError> {
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
            drain_timeout,
            drain_deadline: None,
            drain_expired: false,
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
                let (process, deadline) = {
                    let state = worker_state.lock().expect("incarnation cleanup state");
                    if state.drained() {
                        break;
                    }
                    (state.process.clone(), state.wake_deadline())
                };
                if let Err(error) = receiver.sleep(process.as_ref().map(|process| process.fd()), deadline) {
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
    pub(super) fn release_word(&self, index: usize, field: usize) -> &std::sync::atomic::AtomicU64 {
        assert!(index < self.frames && matches!(field, RELEASE_ID | RELEASE_VALUE | RELEASE_PENDING));
        self.word(HEADER_LEN + index * CLAIM_SLOT_LEN + field)
    }

    pub(super) fn registered_timeline(&self, index: usize) -> &std::sync::atomic::AtomicU64 {
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
        if self.word(ACTIVE).load(SeqCst) == 0 {
            let _ = self.release_wake.signal();
        }
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
        self.collect_retired_allocations()?;
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
    /// Hand this lease to both completion owners. The consumer retains its own
    /// mapping/handles until its bound local completion, then the producer may
    /// return credit after independently observing the registered completion.
    /// Failure returns the original still-owned frame.
    pub fn defer_release(mut self, completion: &super::ConsumerReleaseTimeline, value: u64) -> Result<(), RejectedDeferredRelease> {
        let lifetime = Arc::clone(&self.claim.lifetime);
        match lifetime.retirement.handoff(&mut self.claim, completion, value) {
            Ok(()) => Ok(()),
            Err(error) => Err(RejectedDeferredRelease {
                error,
                frame: Box::new(self),
            }),
        }
    }
}
