use jackstay::acquisition::{AdmissionBook, AdmissionError, AdmissionLimits, HoldingRequest};

#[test]
fn a_paused_replacement_resumes_after_proven_cleanup_and_retries_failed_allocation_without_opening_admission() {
    let mut book = AdmissionBook::new(AdmissionLimits {
        resource_capacity: 10,
        retained_history: 2,
        producer_reserve: 1,
        allocated_bytes: 4096,
        fixed_bytes: 4096,
        memory_budget: 5 * 4096,
        max_incarnations: 4,
    })
    .unwrap();
    let old = book.current_allocation().unwrap();
    book.admit(HoldingRequest {
        frames: 2,
        claim_bytes: 4096,
    })
    .unwrap();
    book.begin_reconfiguration(3 * 4096).unwrap();
    assert_eq!(book.install_reconfiguration(4096), Err(AdmissionError::AllocationNotReserved));
    assert_eq!(
        book.reserve_reconfiguration(),
        Err(AdmissionError::InsufficientMemory {
            requested: 3 * 4096,
            available: 2 * 4096,
        })
    );
    assert_eq!(book.begin_reconfiguration(4096), Err(AdmissionError::ReconfigurationPending));
    assert_eq!(book.committed_bytes(), 3 * 4096);
    book.complete_allocation_cleanup(old).unwrap();
    let failed = book.reserve_reconfiguration().unwrap();
    // The allocator failed and destroyed any partial allocation before this
    // acknowledgement. Retry must not leak bytes or reuse an allocation ID.
    book.abandon_reconfiguration_allocation().unwrap();
    assert_eq!(book.committed_bytes(), 2 * 4096);
    assert_eq!(
        book.admit(HoldingRequest {
            frames: 1,
            claim_bytes: 4096
        }),
        Err(AdmissionError::ReconfigurationPending)
    );
    let replacement = book.reserve_reconfiguration().unwrap();
    assert_ne!(failed, replacement);
    assert_eq!(book.install_reconfiguration(4 * 4096), Err(AdmissionError::InvalidAllocationSize));
    assert_eq!(book.committed_bytes(), 5 * 4096);
    // The native allocator may have reserved a conservative upper bound.
    book.install_reconfiguration(2 * 4096).unwrap();
    assert_eq!(book.committed_bytes(), 4 * 4096);
    book.admit(HoldingRequest {
        frames: 1,
        claim_bytes: 4096,
    })
    .unwrap();
    assert_eq!(book.available_frames(), 4);
}

#[test]
fn replacement_reserves_overlap_bytes_and_blocks_admission_until_installation() {
    let mut book = AdmissionBook::new(AdmissionLimits {
        resource_capacity: 10,
        retained_history: 2,
        producer_reserve: 1,
        allocated_bytes: 4096,
        fixed_bytes: 4096,
        memory_budget: 5 * 4096,
        max_incarnations: 4,
    })
    .unwrap();
    let original = book.current_allocation().unwrap();
    book.admit(HoldingRequest {
        frames: 2,
        claim_bytes: 4096,
    })
    .unwrap();
    book.begin_reconfiguration(2 * 4096).unwrap();
    assert_eq!(book.current_allocation(), None);
    assert_eq!(
        book.admit(HoldingRequest {
            frames: 1,
            claim_bytes: 4096
        }),
        Err(AdmissionError::ReconfigurationPending)
    );
    let replacement = book.reserve_reconfiguration().unwrap();
    assert_eq!(book.reserve_reconfiguration().unwrap(), replacement);
    assert_eq!(book.committed_bytes(), 5 * 4096);
    assert_eq!(book.complete_allocation_cleanup(replacement), Err(AdmissionError::AllocationInUse));
    book.install_reconfiguration(2 * 4096).unwrap();
    assert_eq!(book.current_allocation(), Some(replacement));
    assert_ne!(original, replacement);
    assert_eq!(
        book.admit(HoldingRequest {
            frames: 1,
            claim_bytes: 4096
        }),
        Err(AdmissionError::InsufficientMemory {
            requested: 4096,
            available: 0
        })
    );
    // Installation is not proof that recipients discarded old mappings or
    // that their old leases completed. The old allocation stays charged.
    assert_eq!(book.committed_bytes(), 5 * 4096);
    book.complete_allocation_cleanup(original).unwrap();
    assert_eq!(book.committed_bytes(), 4 * 4096);
    assert_eq!(book.complete_allocation_cleanup(original), Err(AdmissionError::UnknownAllocation));
    assert_eq!(book.complete_allocation_cleanup(replacement), Err(AdmissionError::AllocationInUse));
    book.admit(HoldingRequest {
        frames: 1,
        claim_bytes: 4096,
    })
    .unwrap();
    assert_eq!(book.available_frames(), 4);
}

#[test]
fn restarting_while_old_incarnation_drains_does_not_inherit_or_free_its_reservation() {
    let mut book = AdmissionBook::new(AdmissionLimits {
        resource_capacity: 7,
        retained_history: 2,
        producer_reserve: 1,
        allocated_bytes: 7 * 4096,
        fixed_bytes: 0,
        memory_budget: 10 * 4096,
        max_incarnations: 3,
    })
    .unwrap();
    let old = book
        .admit(HoldingRequest {
            frames: 2,
            claim_bytes: 4096,
        })
        .unwrap();
    assert_eq!(book.complete_cleanup(old.incarnation()), Err(AdmissionError::StillActive));
    book.close(old.incarnation()).unwrap();
    // EOF is not process death or GPU completion. All of the old reservation
    // stays charged while the cleanup owner resolves its outstanding work.
    assert_eq!(book.available_frames(), 2);
    let restarted = book
        .admit(HoldingRequest {
            frames: 2,
            claim_bytes: 4096,
        })
        .unwrap();
    assert_ne!(old.incarnation(), restarted.incarnation());
    assert_eq!(book.available_frames(), 0);
    book.close(old.incarnation()).unwrap();
    assert_eq!(book.available_frames(), 0);

    // Only a cleanup completion acknowledgement returns the old capacity.
    book.complete_cleanup(old.incarnation()).unwrap();
    assert_eq!(book.available_frames(), 2);
    assert_eq!(book.complete_cleanup(old.incarnation()), Err(AdmissionError::UnknownIncarnation));
    assert_eq!(book.available_frames(), 2);
    assert_eq!(book.complete_cleanup(restarted.incarnation()), Err(AdmissionError::StillActive));
}

#[test]
fn admission_reserves_worst_case_distinct_holdings_and_producer_capacity() {
    let mut book = AdmissionBook::new(AdmissionLimits {
        resource_capacity: 12,
        retained_history: 4,
        producer_reserve: 2,
        allocated_bytes: 12 * 4096,
        fixed_bytes: 0,
        memory_budget: 16 * 4096,
        max_incarnations: 4,
    })
    .unwrap();
    let first = book
        .admit(HoldingRequest {
            frames: 4,
            claim_bytes: 4096,
        })
        .unwrap();
    assert_eq!(first.frames(), 4);
    assert_eq!(book.available_frames(), 2);

    // Even if both consumers intend to show the same latest frame, admission
    // must cover them retaining different resources. History and producer
    // working slots cannot be promised to the second consumer.
    assert_eq!(
        book.admit(HoldingRequest {
            frames: 3,
            claim_bytes: 4096
        }),
        Err(AdmissionError::InsufficientResources {
            requested: 3,
            available: 2
        })
    );
    assert_eq!(book.available_frames(), 2);
    let second = book
        .admit(HoldingRequest {
            frames: 2,
            claim_bytes: 4096,
        })
        .unwrap();
    assert_ne!(first.incarnation(), second.incarnation());
    assert_eq!(book.available_frames(), 0);
}

#[test]
fn claim_mapping_memory_and_incarnation_entries_stay_charged_until_cleanup() {
    let mut book = AdmissionBook::new(AdmissionLimits {
        resource_capacity: 100,
        retained_history: 2,
        producer_reserve: 1,
        allocated_bytes: 4096,
        memory_budget: 3 * 4096,
        fixed_bytes: 0,
        max_incarnations: 2,
    })
    .unwrap();
    let first = book
        .admit(HoldingRequest {
            frames: 1,
            claim_bytes: 8192,
        })
        .unwrap();
    book.close(first.incarnation()).unwrap();
    assert_eq!(
        book.admit(HoldingRequest {
            frames: 1,
            claim_bytes: 4096
        }),
        Err(AdmissionError::InsufficientMemory {
            requested: 4096,
            available: 0
        })
    );
    assert_eq!(book.available_frames(), 96);
    book.complete_cleanup(first.incarnation()).unwrap();
    let second = book
        .admit(HoldingRequest {
            frames: 1,
            claim_bytes: 4096,
        })
        .unwrap();
    let third = book
        .admit(HoldingRequest {
            frames: 1,
            claim_bytes: 4096,
        })
        .unwrap();
    assert_ne!(first.incarnation(), third.incarnation());
    book.close(second.incarnation()).unwrap();
    assert_eq!(
        book.admit(HoldingRequest { frames: 1, claim_bytes: 1 }),
        Err(AdmissionError::IncarnationCapacity)
    );
}

#[test]
fn invalid_sizes_cannot_wrap_the_reservation_or_memory_accounting() {
    let mut limits = AdmissionLimits {
        resource_capacity: u32::MAX,
        retained_history: u32::MAX,
        producer_reserve: 1,
        allocated_bytes: 0,
        fixed_bytes: 0,
        memory_budget: u64::MAX,
        max_incarnations: 2,
    };
    assert!(matches!(AdmissionBook::new(limits), Err(AdmissionError::InvalidLimits(_))));
    limits.retained_history = 1;
    let mut book = AdmissionBook::new(limits).unwrap();
    assert_eq!(
        book.admit(HoldingRequest { frames: 0, claim_bytes: 1 }),
        Err(AdmissionError::InvalidRequest)
    );
    assert_eq!(
        book.admit(HoldingRequest { frames: 1, claim_bytes: 0 }),
        Err(AdmissionError::InvalidRequest)
    );
    book.admit(HoldingRequest {
        frames: 1,
        claim_bytes: u64::MAX,
    })
    .unwrap();
    assert_eq!(
        book.admit(HoldingRequest { frames: 1, claim_bytes: 1 }),
        Err(AdmissionError::InsufficientMemory {
            requested: 1,
            available: 0
        })
    );
}
