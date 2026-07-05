use std::collections::{HashMap, HashSet};

use thiserror::Error;

/// The frame identity protected by one native lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeLeaseIdentity {
    pub cursor: u64,
    pub sequence: u64,
    pub pool_id: u64,
    pub slot_id: u32,
}

/// How a consumer releases a lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeLeaseRelease {
    /// The consumer is fully done with the surface now.
    Now,
    /// The consumer will be done once its registered release timeline reaches
    /// `value`.
    TimelineValue { release_sync_id: u64, value: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleasedNativeLease {
    pub lease_id: u64,
    pub identity: NativeLeaseIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum NativeLeaseError {
    #[error("native lease id {0} is unknown")]
    UnknownLease(u64),
    #[error("native lease id {0} was already released")]
    AlreadyReleased(u64),
    #[error("native release sync id {0} is unknown")]
    UnknownReleaseSync(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeaseState {
    Outstanding,
    ReleasePending { release_sync_id: u64, value: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LeaseEntry {
    identity: NativeLeaseIdentity,
    state: LeaseState,
}

/// Tracks native frame leases and release timelines. This is transport-neutral:
/// the caller owns the actual sync handles and tells the book when a release
/// timeline has reached a value.
#[derive(Debug, Default)]
pub struct NativeLeaseBook {
    next_lease_id: u64,
    next_release_sync_id: u64,
    leases: HashMap<u64, LeaseEntry>,
    release_syncs: HashSet<u64>,
}

impl NativeLeaseBook {
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_lease_id: 1,
            next_release_sync_id: 1,
            leases: HashMap::new(),
            release_syncs: HashSet::new(),
        }
    }

    pub fn acquire(&mut self, identity: NativeLeaseIdentity) -> u64 {
        let lease_id = self.next_lease_id;
        self.next_lease_id = self.next_lease_id.saturating_add(1);
        self.leases.insert(
            lease_id,
            LeaseEntry {
                identity,
                state: LeaseState::Outstanding,
            },
        );
        lease_id
    }

    pub fn register_release_sync(&mut self) -> u64 {
        let release_sync_id = self.next_release_sync_id;
        self.next_release_sync_id = self.next_release_sync_id.saturating_add(1);
        self.release_syncs.insert(release_sync_id);
        release_sync_id
    }

    pub fn register_release_sync_id(&mut self, release_sync_id: u64) {
        self.next_release_sync_id = self.next_release_sync_id.max(release_sync_id.saturating_add(1));
        self.release_syncs.insert(release_sync_id);
    }

    pub fn release(&mut self, lease_id: u64, release: NativeLeaseRelease) -> Result<ReleasedNativeLease, NativeLeaseError> {
        let Some(mut entry) = self.leases.remove(&lease_id) else {
            return Err(NativeLeaseError::UnknownLease(lease_id));
        };
        if entry.state != LeaseState::Outstanding {
            self.leases.insert(lease_id, entry);
            return Err(NativeLeaseError::AlreadyReleased(lease_id));
        }
        match release {
            NativeLeaseRelease::Now => Ok(ReleasedNativeLease {
                lease_id,
                identity: entry.identity,
            }),
            NativeLeaseRelease::TimelineValue { release_sync_id, value } => {
                if !self.release_syncs.contains(&release_sync_id) {
                    self.leases.insert(lease_id, entry);
                    return Err(NativeLeaseError::UnknownReleaseSync(release_sync_id));
                }
                entry.state = LeaseState::ReleasePending { release_sync_id, value };
                self.leases.insert(lease_id, entry);
                Ok(ReleasedNativeLease {
                    lease_id,
                    identity: entry.identity,
                })
            }
        }
    }

    pub fn complete_release_sync(&mut self, release_sync_id: u64, reached_value: u64) -> Result<(), NativeLeaseError> {
        if !self.release_syncs.contains(&release_sync_id) {
            return Err(NativeLeaseError::UnknownReleaseSync(release_sync_id));
        }
        self.leases.retain(|_, entry| {
            !matches!(
                entry.state,
                LeaseState::ReleasePending {
                    release_sync_id: pending_sync_id,
                    value
                } if pending_sync_id == release_sync_id && value <= reached_value
            )
        });
        Ok(())
    }

    #[must_use]
    pub fn slot_has_unresolved_leases(&self, pool_id: u64, slot_id: u32) -> bool {
        self.leases
            .values()
            .any(|entry| entry.identity.pool_id == pool_id && entry.identity.slot_id == slot_id)
    }

    #[must_use]
    pub fn outstanding_count(&self) -> usize {
        self.leases.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{NativeLeaseBook, NativeLeaseError, NativeLeaseIdentity, NativeLeaseRelease};

    fn identity(slot_id: u32) -> NativeLeaseIdentity {
        NativeLeaseIdentity {
            cursor: slot_id as u64 + 1,
            sequence: slot_id as u64 + 10,
            pool_id: 7,
            slot_id,
        }
    }

    #[test]
    fn release_now_resolves_one_lease_and_rejects_double_release() {
        let mut book = NativeLeaseBook::new();
        let lease_id = book.acquire(identity(2));
        assert!(book.slot_has_unresolved_leases(7, 2));

        let released = book.release(lease_id, NativeLeaseRelease::Now).unwrap();
        assert_eq!(released.identity, identity(2));
        assert!(!book.slot_has_unresolved_leases(7, 2));

        assert_eq!(
            book.release(lease_id, NativeLeaseRelease::Now),
            Err(NativeLeaseError::UnknownLease(lease_id))
        );
    }

    #[test]
    fn multiple_leases_on_one_slot_must_all_resolve() {
        let mut book = NativeLeaseBook::new();
        let first = book.acquire(identity(1));
        let second = book.acquire(identity(1));

        book.release(first, NativeLeaseRelease::Now).unwrap();
        assert!(book.slot_has_unresolved_leases(7, 1));

        book.release(second, NativeLeaseRelease::Now).unwrap();
        assert!(!book.slot_has_unresolved_leases(7, 1));
    }

    #[test]
    fn timeline_release_keeps_slot_unresolved_until_sync_completes() {
        let mut book = NativeLeaseBook::new();
        let release_sync_id = book.register_release_sync();
        let lease_id = book.acquire(identity(3));

        book.release(lease_id, NativeLeaseRelease::TimelineValue { release_sync_id, value: 5 })
            .unwrap();
        assert!(book.slot_has_unresolved_leases(7, 3));
        assert_eq!(
            book.release(lease_id, NativeLeaseRelease::Now),
            Err(NativeLeaseError::AlreadyReleased(lease_id))
        );

        book.complete_release_sync(release_sync_id, 4).unwrap();
        assert!(book.slot_has_unresolved_leases(7, 3));

        book.complete_release_sync(release_sync_id, 5).unwrap();
        assert!(!book.slot_has_unresolved_leases(7, 3));
    }

    #[test]
    fn externally_assigned_release_sync_id_can_be_registered() {
        let mut book = NativeLeaseBook::new();
        book.register_release_sync_id(42);
        let lease_id = book.acquire(identity(1));

        book.release(
            lease_id,
            NativeLeaseRelease::TimelineValue {
                release_sync_id: 42,
                value: 9,
            },
        )
        .unwrap();
        assert!(book.slot_has_unresolved_leases(7, 1));

        book.complete_release_sync(42, 9).unwrap();
        assert!(!book.slot_has_unresolved_leases(7, 1));
        assert_eq!(book.register_release_sync(), 43);
    }
}
