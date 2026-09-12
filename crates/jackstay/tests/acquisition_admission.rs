use jackstay::acquisition::{AdmissionBook, AdmissionError, AdmissionLimits, HoldingRequest};

#[test]
fn restarting_while_old_incarnation_drains_does_not_inherit_or_free_its_reservation() {
    let mut book = AdmissionBook::new(AdmissionLimits {
        resource_capacity: 7,
        retained_history: 2,
        producer_reserve: 1,
        allocated_bytes: 7 * 4096,
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
