use std::{
    collections::BTreeMap,
    fmt::Debug,
    sync::{Arc, atomic::Ordering::SeqCst},
};

use serde::{Deserialize, Serialize};

use super::{
    ACTIVE, AdmissionError, ArenaError, ArenaProducer, CAPACITY_EPOCH, CLAIM_SLOT_LEN, ClaimMap, FrameLease, HEADER_LEN, IncarnationId,
    RELEASE_ID, RELEASE_PENDING, RELEASE_VALUE, wait,
};

/// Producer-owned access to a registered native completion timeline. Returning a
/// value asserts that the corresponding consumer GPU work has completed, not
/// merely been submitted. The imported handle must remain alive through draining.
pub trait ReleaseTimeline: Send + Sync + Debug {
    fn completed_value(&self) -> crate::Result<u64>;
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

#[derive(Debug, Default)]
pub(super) struct ReleaseRegistry {
    timelines: BTreeMap<IncarnationId, Vec<Arc<dyn ReleaseTimeline>>>,
    failures: BTreeMap<IncarnationId, String>,
}

impl ReleaseRegistry {
    pub(super) fn remove(&mut self, incarnation: IncarnationId) {
        self.timelines.remove(&incarnation);
        self.failures.remove(&incarnation);
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
    pub fn release_recovery_failures(&self) -> Vec<ReleaseRecoveryFailure> {
        self.releases
            .failures
            .iter()
            .map(|(incarnation, reason)| ReleaseRecoveryFailure {
                incarnation: *incarnation,
                reason: reason.clone(),
            })
            .collect()
    }

    /// Retry observing a retained completion source after the host repairs its
    /// backend. This neither reopens acquisition nor force-releases any claim.
    pub fn retry_release_cleanup(&mut self, incarnation: IncarnationId) -> Result<(), ArenaError> {
        if !self.claims.contains_key(&incarnation) {
            return Err(AdmissionError::UnknownIncarnation.into());
        }
        self.releases.failures.remove(&incarnation);
        Ok(())
    }
    /// Import each consumer release timeline once through setup. Registrations
    /// are bounded by the incarnation's holding reservation and remain alive
    /// until its claims and mapping have been reclaimed.
    pub fn register_release_timeline(
        &mut self,
        incarnation: IncarnationId,
        timeline: Arc<dyn ReleaseTimeline>,
    ) -> Result<ReleaseTimelineRegistration, ArenaError> {
        let claims = self.claims.get(&incarnation).ok_or(AdmissionError::UnknownIncarnation)?;
        if claims.word(ACTIVE).load(SeqCst) == 0 {
            return Err(ArenaError::Closed);
        }
        let timelines = self.releases.timelines.entry(incarnation).or_default();
        if timelines.len() >= claims.frames {
            return Err(ArenaError::Configuration(
                "release timeline capacity equals the holding reservation",
            ));
        }
        let index = timelines.len();
        let id = index as u64 + 1;
        timelines.push(timeline);
        claims.registered_timeline(index).store(id, SeqCst);
        Ok(ReleaseTimelineRegistration {
            incarnation: incarnation.0,
            scope: claims.scope,
            id,
        })
    }

    /// Reap only completion values reported by producer-owned imported handles.
    /// Publication and admission call this too. Native integration must also
    /// drive this when production is paused; a submitted release is not itself
    /// a completion event. Consumer shutdown never clears a pending claim.
    pub fn poll_release_completions(&mut self) -> Result<usize, ArenaError> {
        let mut released = 0;
        for (incarnation, claims) in &self.claims {
            if self.releases.failures.contains_key(incarnation) {
                continue;
            }
            for index in 0..claims.frames {
                match claims.release_word(index, RELEASE_PENDING).load(SeqCst) {
                    0 => continue,
                    1 => {}
                    _ => {
                        claims.close();
                        self.releases
                            .failures
                            .insert(*incarnation, "invalid deferred release state".to_owned());
                        break;
                    }
                }
                let id = claims.release_word(index, RELEASE_ID).load(SeqCst);
                let value = claims.release_word(index, RELEASE_VALUE).load(SeqCst);
                let timeline = self
                    .releases
                    .timelines
                    .get(incarnation)
                    .and_then(|timelines| id.checked_sub(1).and_then(|id| timelines.get(id as usize)));
                let Some(timeline) = timeline else {
                    claims.close();
                    self.releases
                        .failures
                        .insert(*incarnation, "deferred release names an unregistered timeline".to_owned());
                    break;
                };
                let completed = match timeline.completed_value() {
                    Ok(value) => value,
                    Err(error) => {
                        claims.close();
                        self.releases.failures.insert(*incarnation, error.to_string());
                        break;
                    }
                };
                if completed >= value {
                    claims.return_credit(index);
                    released += 1;
                }
            }
        }
        self.collect_quiescent();
        Ok(released)
    }
}

#[derive(Debug, Clone)]
pub struct ReleaseRecoveryFailure {
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
        Ok(())
    }
}
