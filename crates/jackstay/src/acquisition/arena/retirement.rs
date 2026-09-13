//! Consumer-local completion ownership. It must survive either public API owner
//! being destroyed and cannot depend on the producer continuing to observe work.
use std::{
    sync::{Arc, Condvar, Mutex, Weak, atomic::Ordering::SeqCst},
    time::{Duration, Instant},
};

use super::{
    ArenaConsumer, ArenaError, Claim, ClaimMap, ConsumerResources, RELEASE_ID, RELEASE_PENDING, RELEASE_VALUE, ReleaseNotification,
    ReleaseTimeline, ReleaseTimelineRegistration,
};

#[derive(Debug, Default)]
pub(super) struct LocalWake {
    signaled: Mutex<bool>,
    changed: Condvar,
}

impl LocalWake {
    pub(super) fn signal(&self) {
        *self.signaled.lock().expect("consumer retirement wake") = true;
        self.changed.notify_one();
    }

    fn drain(&self) {
        *self.signaled.lock().expect("consumer retirement wake") = false;
    }

    fn wait(&self, deadline: Option<Instant>) {
        let mut signaled = self.signaled.lock().expect("consumer retirement wake");
        while !*signaled {
            match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return;
                    }
                    let (guard, timeout) = self.changed.wait_timeout(signaled, remaining).expect("consumer retirement wake");
                    signaled = guard;
                    if timeout.timed_out() {
                        return;
                    }
                }
                None => signaled = self.changed.wait(signaled).expect("consumer retirement wake"),
            }
        }
    }
}

#[derive(Debug, Default)]
struct Pending {
    resources: Option<Arc<ConsumerResources>>,
    timeline: usize,
    value: u64,
    armed: Option<ReleaseNotification>,
}

#[derive(Debug)]
struct State {
    claims: Arc<ClaimMap>,
    sources: Vec<Option<Arc<dyn ReleaseTimeline>>>,
    slots: Vec<Pending>,
    wake: Arc<LocalWake>,
    closed: bool,
    failure: Option<String>,
    drain_timeout: Duration,
    deadline: Option<Instant>,
}

impl State {
    fn empty(&self) -> bool {
        self.slots.iter().all(|slot| slot.resources.is_none())
    }

    fn failure_reason(&self) -> Option<String> {
        if self.empty() {
            return None;
        }
        self.failure.clone().or_else(|| {
            self.deadline
                .filter(|deadline| Instant::now() >= *deadline)
                .map(|_| "consumer retirement deadline expired; mappings remain retained".to_owned())
        })
    }

    fn fail(&mut self, error: impl ToString) {
        self.failure = Some(error.to_string());
        self.claims.close();
    }

    fn poll(&mut self) {
        if self.failure.is_some() {
            return;
        }
        for index in 0..self.slots.len() {
            let slot = &mut self.slots[index];
            if slot.armed.as_ref().is_some_and(ReleaseNotification::has_fired) {
                slot.armed = None;
            }
            if slot.resources.is_none() {
                continue;
            }
            let source = self.sources[slot.timeline].as_ref().expect("bound completion source");
            let completed = match source.completed_value() {
                Ok(value) => value,
                Err(error) => {
                    self.fail(error);
                    return;
                }
            };
            if completed >= slot.value {
                // Unmap/release consumer handles before allowing the producer
                // to return holding credit. The producer still checks its own
                // imported completion source before clearing this claim.
                drop(slot.resources.take());
                self.claims.release_word(index, RELEASE_PENDING).store(2, SeqCst);
                let _ = self.claims.release_wake.signal();
            } else if slot.armed.is_none() {
                let notification = ReleaseNotification::for_local(Arc::clone(&self.wake));
                if let Err(error) = source.notify_at(slot.value, notification.clone()) {
                    self.fail(error);
                    return;
                }
                slot.armed = Some(notification);
            }
        }
        if self.closed && self.empty() {
            self.claims.acknowledge_quiescent();
        }
    }
}

#[derive(Debug)]
pub(super) struct Retirement {
    state: Arc<Mutex<State>>,
    wake: Arc<LocalWake>,
}

impl Retirement {
    pub(super) fn new(claims: Arc<ClaimMap>, drain_timeout: Duration) -> Result<Self, ArenaError> {
        let wake = Arc::new(LocalWake::default());
        let state = Arc::new(Mutex::new(State {
            slots: (0..claims.frames).map(|_| Pending::default()).collect(),
            sources: (0..claims.frames).map(|_| None).collect(),
            claims,
            wake: Arc::clone(&wake),
            closed: false,
            failure: None,
            drain_timeout,
            deadline: None,
        }));
        let worker_state = Arc::clone(&state);
        let worker_wake = Arc::clone(&wake);
        std::thread::Builder::new().name("jackstay-retire".to_owned()).spawn(move || {
            loop {
                worker_wake.drain();
                let deadline = {
                    let mut state = worker_state.lock().expect("consumer retirement state");
                    state.poll();
                    if state.closed && state.empty() {
                        break;
                    }
                    state.deadline.filter(|deadline| *deadline > Instant::now())
                };
                worker_wake.wait(deadline);
            }
        })?;
        Ok(Self { state, wake })
    }

    pub(super) fn handoff(&self, claim: &mut Claim, binding: &ConsumerReleaseTimeline, value: u64) -> Result<(), ArenaError> {
        if !Weak::ptr_eq(&Arc::downgrade(&self.state), &binding.state) {
            return Err(ArenaError::Mapping("consumer completion belongs to another incarnation"));
        }
        let mut state = self.state.lock().expect("consumer retirement state");
        if let Some(reason) = state.failure_reason() {
            return Err(ArenaError::RecoveryRequired { reason });
        }
        let slot = &mut state.slots[claim.slot];
        if slot.resources.is_some() {
            return Err(ArenaError::Mapping("consumer retirement slot is still in use"));
        }
        slot.resources = claim.owner.take();
        slot.timeline = binding.index;
        slot.value = value;
        claim.release_on_drop = false;
        state
            .claims
            .release_word(claim.slot, RELEASE_ID)
            .store(binding.registration.id, SeqCst);
        state.claims.release_word(claim.slot, RELEASE_VALUE).store(value, SeqCst);
        state.claims.release_word(claim.slot, RELEASE_PENDING).store(1, SeqCst);
        // The same mutex excludes an already-complete local observer until the
        // handoff is published. It cannot publish state 2 before this state 1.
        let _ = state.claims.release_wake.signal();
        drop(state);
        self.wake.signal();
        Ok(())
    }

    pub(super) fn begin_drain(&self) {
        let mut state = self.state.lock().expect("consumer retirement state");
        state.claims.close();
        if state.deadline.is_none() {
            state.deadline = Instant::now().checked_add(state.drain_timeout);
        }
        drop(state);
        self.wake.signal();
    }

    pub(super) fn close(&self) {
        self.begin_drain();
        let mut state = self.state.lock().expect("consumer retirement state");
        state.closed = true;
        state.poll();
        drop(state);
        self.wake.signal();
    }
}

/// Consumer-local binding to the same actual completion timeline registered by
/// the producer. This handle does not keep completed mappings or an incarnation
/// alive. Pending use has a separate bounded owner even after this handle drops.
#[derive(Debug, Clone)]
pub struct ConsumerReleaseTimeline {
    registration: ReleaseTimelineRegistration,
    index: usize,
    state: Weak<Mutex<State>>,
    wake: Arc<LocalWake>,
}

impl ConsumerReleaseTimeline {
    /// Consumer mappings awaiting completion on this binding. Producer holding
    /// credit may remain charged until its separate observer finishes too.
    #[must_use]
    pub fn pending_releases(&self) -> usize {
        self.state.upgrade().map_or(0, |state| {
            state
                .lock()
                .expect("consumer retirement state")
                .slots
                .iter()
                .filter(|slot| slot.resources.is_some() && slot.timeline == self.index)
                .count()
        })
    }

    #[must_use]
    pub fn cleanup_failure(&self) -> Option<String> {
        self.state
            .upgrade()
            .and_then(|state| state.lock().expect("consumer retirement state").failure_reason())
    }

    pub fn retry_cleanup(&self) -> Result<(), ArenaError> {
        let Some(state) = self.state.upgrade() else {
            return Ok(());
        };
        let mut state = state.lock().expect("consumer retirement state");
        state.failure = None;
        self.wake.signal();
        if let Some(reason) = state.failure_reason() {
            return Err(ArenaError::RecoveryRequired { reason });
        }
        Ok(())
    }
}

impl ArenaConsumer {
    /// Bind before asynchronous use. `source` must observe the same actual
    /// completion signal as the producer's imported registration. Both ends
    /// retain their own handle; producer shutdown is not completion evidence.
    pub fn bind_release_timeline(
        &self,
        registration: &ReleaseTimelineRegistration,
        source: Arc<dyn ReleaseTimeline>,
    ) -> Result<ConsumerReleaseTimeline, ArenaError> {
        let claims = &self.lifetime.claims;
        if registration.incarnation != claims.incarnation.0
            || registration.scope != claims.scope
            || registration.id == 0
            || registration.id > claims.frames as u64
            || claims.registered_timeline(registration.id as usize - 1).load(SeqCst) != registration.id
        {
            return Err(ArenaError::Mapping(
                "release timeline belongs to another incarnation or is unregistered",
            ));
        }
        if self.is_closed() {
            return Err(ArenaError::Closed);
        }
        let index = registration.id as usize - 1;
        let mut state = self.lifetime.retirement.state.lock().expect("consumer retirement state");
        if let Some(existing) = &state.sources[index] {
            if !Arc::ptr_eq(existing, &source) {
                return Err(ArenaError::Configuration("release timeline already has a local binding"));
            }
        } else {
            state.sources[index] = Some(source);
        }
        Ok(ConsumerReleaseTimeline {
            registration: registration.clone(),
            index,
            state: Arc::downgrade(&self.lifetime.retirement.state),
            wake: Arc::clone(&self.lifetime.retirement.wake),
        })
    }
}
