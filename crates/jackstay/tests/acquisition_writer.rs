use std::{process::Command, time::Duration};

use jackstay::acquisition::arena::*;
#[path = "support/setup.rs"]
mod setup;
fn config() -> ArenaConfig {
    ArenaConfig {
        resource_capacity: 4,
        retained_history: 1,
        producer_reserve: 1,
        payload_capacity: 16,
        memory_budget: 1 << 20,
        max_incarnations: 1,
        drain_timeout: Duration::from_secs(5),
    }
}
fn descriptor() -> FrameDescriptor {
    FrameDescriptor {
        payload_len: 16,
        width: 4,
        height: 1,
        stride: 16,
        ..Default::default()
    }
}

// Issue #75: reserving must exclude live consumer leases and other reservations;
// abandoning returns a slot, and copy-in publication must skip reservations.
#[test]
fn reservations_exclude_leases_and_each_other() {
    let mut producer = ArenaProducer::new(config()).unwrap();
    let consumer = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    producer.publish(descriptor(), &[7; 16]).unwrap();
    let AcquireOutcome::Frame(held) = consumer.acquire_latest(0).unwrap() else {
        panic!()
    };
    producer.publish(descriptor(), &[8; 16]).unwrap();
    let mut reserved = Vec::new();
    while let Some(mut slot) = producer.reserve().unwrap() {
        assert_ne!(slot.slot().slot, held.descriptor().slot_id);
        slot.bytes_mut().fill(9);
        reserved.push(slot);
    }
    assert_eq!(reserved.len(), 2);
    assert_eq!(producer.publish(descriptor(), &[10; 16]).unwrap(), PublishOutcome::Dropped);
    assert_eq!(held.bytes(), &[7; 16]);
    producer.abandon(reserved.pop().unwrap()).unwrap();
    let slot = producer.reserve().unwrap().unwrap();
    producer.commit(slot, descriptor()).unwrap();
    assert_eq!(held.bytes(), &[7; 16]);
    drop(reserved);
    assert!(producer.reserve().unwrap().is_some());
}

// Issue #75: reconfiguration invalidates commits but retains reserved storage
// and exports in accounting. Paused allocations hand out neither slots nor maps.
#[test]
fn reconfiguration_retains_writer_owners_and_refuses_stale_slots() {
    let mut cfg = config();
    cfg.memory_budget = 32 * 1024;
    let mut producer = ArenaProducer::new(cfg).unwrap();
    let reservation = producer.reserve().unwrap().unwrap();
    let export = producer.export_writer().unwrap().unwrap();
    let old = reservation.slot();
    assert!(matches!(
        producer.reconfigure_cpu(6 * 1024).unwrap(),
        ReconfigurationStatus::PausedCapacity { .. }
    ));
    assert!(producer.reserve().unwrap().is_none());
    assert!(producer.export_writer().unwrap().is_none());
    assert!(producer.commit(reservation, descriptor()).is_err());
    assert!(matches!(
        producer.advance_reconfiguration().unwrap(),
        ReconfigurationStatus::PausedCapacity { .. }
    ));
    drop(export);
    assert!(matches!(
        producer.advance_reconfiguration().unwrap(),
        ReconfigurationStatus::Ready { .. }
    ));
    let mut producer = ArenaProducer::new(config()).unwrap();
    let export = producer.export_writer().unwrap().unwrap();
    // SAFETY: live export retained; old mapping touched only without any lease.
    let mut writer = unsafe { DelegatedWriter::from_parts(export.descriptor(), export.duplicate_object().unwrap()) }.unwrap();
    let mut slot = producer.reserve().unwrap().unwrap();
    let old_slot = slot.slot();
    writer_bytes(&mut writer, old_slot).fill(42);
    producer.reconfigure_cpu(16).unwrap();
    assert!(producer.commit(slot, descriptor()).is_err());
    slot = producer.reserve().unwrap().unwrap();
    slot.bytes_mut().fill(17);
    assert!(unsafe { writer.bytes_mut(slot.slot()) }.is_err());
    assert!(unsafe { writer.bytes_mut(old) }.is_err()); // foreign arena
    writer_bytes(&mut writer, old_slot).fill(99); // old object is harmless
    producer.commit(slot, descriptor()).unwrap();
    let consumer = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    let AcquireOutcome::Frame(frame) = consumer.acquire_latest(0).unwrap() else {
        panic!()
    };
    assert_eq!(frame.bytes(), &[17; 16]);
    drop(writer);
    drop(export);
}
fn writer_bytes(writer: &mut DelegatedWriter, slot: WriterSlot) -> &mut [u8] {
    // SAFETY: tests serialize producer and delegate and retain all export owners.
    unsafe { writer.bytes_mut(slot) }.unwrap()
}

// Issue #75: a real child maps payload export writable, fills only its assigned
// slot, acknowledges completion, and a consumer observes those exact bytes.
#[test]
fn child_writer_publishes_byte_exact_frames() {
    subprocess(false);
}
#[test]
fn c_child_writer_publishes_byte_exact_frames() {
    subprocess_c();
}
fn subprocess_c() {
    use jackstay::{
        ffi::*,
        ffi_acquisition::{producer::*, *},
    };
    let listener = setup::Listener::bind("c-producer-writer");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "writer_child"])
        .env("JACKSTAY_WRITER_SOCKET", listener.address())
        .env("JACKSTAY_WRITER_C", "1")
        .spawn()
        .unwrap();
    // SAFETY: all C handles are exclusively owned and outputs disjoint. Export
    // is retained until child exit; no local slot view is used during its writes.
    unsafe {
        let mut producer = std::ptr::null_mut();
        let cfg = FtCpuProducerConfig {
            resource_capacity: 4,
            retained_history: 1,
            producer_reserve: 1,
            max_incarnations: 1,
            payload_capacity: 16,
            memory_budget: 1 << 20,
            drain_timeout_ns: 5_000_000_000,
        };
        assert_eq!(ft_cpu_producer_create(&cfg, &mut producer), FT_STATUS_OK);
        let mut export = std::ptr::null_mut();
        let mut object = FT_OS_OBJECT_NONE;
        let mut layout = WriterDescriptor {
            arena_scope: [0; 16],
            generation: 0,
            map_len: 0,
            slot_capacity: 0,
            slots: 0,
        };
        assert_eq!(
            ft_cpu_producer_export_writer(producer, &mut export, &mut layout, &mut object),
            FT_STATUS_OK
        );
        let mut reservation = std::ptr::null_mut();
        let mut bytes = std::ptr::null_mut();
        let mut len = 0;
        let mut slot = WriterSlot {
            arena_scope: [0; 16],
            generation: 0,
            slot: 0,
        };
        assert_eq!(
            ft_cpu_producer_reserve(producer, &mut reservation, &mut bytes, &mut len, &mut slot),
            FT_STATUS_OK
        );
        #[cfg(unix)]
        let object = {
            use std::os::fd::{FromRawFd, OwnedFd};
            OwnedFd::from_raw_fd(object)
        };
        #[cfg(windows)]
        let object = {
            use std::os::windows::io::{FromRawHandle, OwnedHandle};
            OwnedHandle::from_raw_handle(object)
        };
        let mut link = listener.accept();
        link.send(&layout);
        link.send_objects(&child, &[object]);
        link.send(&slot);
        assert_eq!(link.read_byte(), 1);
        let mut descriptor = descriptor();
        descriptor.pixel_format = FT_PIXEL_FORMAT_BGRA8_UNORM;
        let mut cursor = 0;
        assert_eq!(ft_cpu_producer_commit(&mut reservation, &descriptor, &mut cursor), FT_STATUS_OK);
        let mut consumer = std::ptr::null_mut();
        let mut frame = std::ptr::null_mut();
        let mut range = FtAcquisitionRange::default();
        assert_eq!(ft_cpu_producer_attach(producer, 1, &mut consumer), FT_STATUS_OK);
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut frame, &mut range),
            FT_STATUS_OK
        );
        let mut data = std::ptr::null();
        assert_eq!(ft_acquired_frame_bytes(frame, &mut data, &mut len), FT_STATUS_OK);
        assert_eq!(std::slice::from_raw_parts(data, len), &(0..16).map(|x| x * 11).collect::<Vec<u8>>());
        assert!(child.wait().unwrap().success());
        assert_eq!(ft_cpu_writer_export_destroy(&mut export), FT_STATUS_OK);
        ft_acquired_frame_release(&mut frame);
        ft_acquisition_consumer_destroy(&mut consumer);
        assert_eq!(ft_cpu_producer_destroy(&mut producer), FT_STATUS_OK);
    }
}
fn subprocess(c_abi: bool) {
    let listener = setup::Listener::bind(if c_abi { "c-writer" } else { "writer" });
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "writer_child"])
        .env("JACKSTAY_WRITER_SOCKET", listener.address())
        .env("JACKSTAY_WRITER_C", if c_abi { "1" } else { "0" })
        .spawn()
        .unwrap();
    let mut producer = ArenaProducer::new(config()).unwrap();
    let export = producer.export_writer().unwrap().unwrap();
    let reservation = producer.reserve().unwrap().unwrap();
    let mut link = listener.accept();
    link.send(&export.descriptor());
    // SAFETY: export retained until child exits and object is dropped below.
    let object = unsafe { export.duplicate_object() }.unwrap();
    link.send_objects(&child, &[object]);
    link.send(&reservation.slot());
    assert_eq!(link.read_byte(), 1);
    producer.commit(reservation, descriptor()).unwrap();
    let consumer = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    let AcquireOutcome::Frame(frame) = consumer.acquire_latest(0).unwrap() else {
        panic!()
    };
    assert_eq!(frame.bytes(), &(0..16).map(|x| x * 11).collect::<Vec<u8>>());
    assert!(child.wait().unwrap().success());
    drop(export);
}
#[test]
#[ignore = "subprocess helper"]
fn writer_child() {
    let mut link = setup::Link::connect(&std::env::var("JACKSTAY_WRITER_SOCKET").unwrap());
    let descriptor: WriterDescriptor = link.recv();
    let object = link.recv_objects(1).pop().unwrap();
    let slot: WriterSlot = link.recv();
    if std::env::var("JACKSTAY_WRITER_C").unwrap() == "1" {
        use jackstay::{
            ffi::*,
            ffi_acquisition::{producer::*, *},
        };
        #[cfg(unix)]
        let mut raw = {
            use std::os::fd::IntoRawFd;
            object.into_raw_fd()
        };
        #[cfg(windows)]
        let mut raw = {
            use std::os::windows::io::IntoRawHandle;
            object.into_raw_handle()
        };
        let mut writer = std::ptr::null_mut();
        let mut bytes = std::ptr::null_mut();
        let mut len = 0;
        // SAFETY: parent's live export/reservation is assigned to this child;
        // pointers are exclusive, views end before completion acknowledgement.
        unsafe {
            assert_eq!(ft_cpu_writer_import(&descriptor, &mut raw, &mut writer), FT_STATUS_OK);
            assert_eq!(raw, FT_OS_OBJECT_NONE);
            let mut stale = slot;
            stale.generation += 1;
            assert_eq!(ft_cpu_writer_slot_view(writer, &stale, &mut bytes, &mut len), FT_STATUS_STALE);
            assert_eq!(ft_cpu_writer_slot_view(writer, &slot, &mut bytes, &mut len), FT_STATUS_OK);
            for (i, byte) in std::slice::from_raw_parts_mut(bytes, len).iter_mut().enumerate() {
                *byte = i as u8 * 11;
            }
            assert_eq!(ft_cpu_writer_destroy(&mut writer), FT_STATUS_OK);
        }
    } else {
        // SAFETY: parent's export owner lives until this child exits.
        let mut writer = unsafe { DelegatedWriter::from_parts(descriptor, object) }.unwrap();
        for (i, byte) in writer_bytes(&mut writer, slot).iter_mut().enumerate() {
            *byte = i as u8 * 11;
        }
        drop(writer);
    }
    link.write_byte(1);
}

// Issue #75 ownership invariant: generated operation sequences must never
// change bytes under a live lease. Seeds cover reserve/commit/abandon, copy-in,
// acquire/release, and reconfiguration interleavings with empty/full slot sets.
#[test]
fn generated_slot_lifecycles_preserve_leased_bytes() {
    for seed in 1..=32u64 {
        let mut random = seed;
        let mut producer = ArenaProducer::new(config()).unwrap();
        let mut consumer = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
        let mut reservation = None;
        let mut lease: Option<(FrameLease, Vec<u8>)> = None;
        for _ in 0..256 {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let value = (random >> 32) as u8;
            match (random >> 32) % 7 {
                0 if reservation.is_none() => {
                    reservation = producer.reserve().unwrap();
                }
                1 => {
                    if let Some(mut slot) = reservation.take() {
                        slot.bytes_mut().fill(value);
                        // A reconfiguration may have invalidated this owner.
                        let _ = producer.commit(slot, descriptor());
                    }
                }
                2 => {
                    if let Some(slot) = reservation.take() {
                        producer.abandon(slot).unwrap();
                    }
                }
                3 => {
                    producer.publish(descriptor(), &[value; 16]).unwrap();
                }
                4 if lease.is_none() => {
                    if let AcquireOutcome::Frame(frame) = consumer.acquire_latest(0).unwrap() {
                        let expected = frame.bytes().to_vec();
                        lease = Some((frame, expected));
                    }
                }
                5 => {
                    lease = None;
                }
                6 => {
                    producer.reconfigure_cpu(16).unwrap();
                    producer.configure_consumer(&mut consumer).unwrap();
                }
                _ => {}
            }
            if let Some((frame, expected)) = &lease {
                assert_eq!(frame.bytes(), expected);
            }
        }
    }
}

// Export validation rejects empty layouts and out-of-bounds/foreign instructions;
// commits reject oversized payloads and reservations from another arena.
#[test]
fn invalid_writer_layouts_slots_and_commits_are_rejected() {
    let mut producer = ArenaProducer::new(config()).unwrap();
    let export = producer.export_writer().unwrap().unwrap();
    let mut layout = export.descriptor();
    layout.slots = 0;
    // SAFETY: actual live producer object; malformed metadata must be rejected
    // before a writable view escapes. Export owner outlives all imported views.
    assert!(unsafe { DelegatedWriter::from_parts(layout, export.duplicate_object().unwrap()) }.is_err());
    let mut writer = unsafe { DelegatedWriter::from_parts(export.descriptor(), export.duplicate_object().unwrap()) }.unwrap();
    let slot = producer.reserve().unwrap().unwrap();
    let mut invalid = slot.slot();
    invalid.slot = u32::MAX;
    assert!(unsafe { writer.bytes_mut(invalid) }.is_err());
    let mut invalid = slot.slot();
    invalid.generation = 0;
    assert!(unsafe { writer.bytes_mut(invalid) }.is_err());
    let mut descriptor = descriptor();
    descriptor.payload_len = 17;
    assert!(matches!(producer.commit(slot, descriptor), Err(ArenaError::PayloadTooLarge)));
    let mut foreign = ArenaProducer::new(config()).unwrap();
    let slot = producer.reserve().unwrap().unwrap();
    assert!(foreign.commit(slot, descriptor).is_err());
    let slot = producer.reserve().unwrap().unwrap();
    assert!(foreign.abandon(slot).is_err());
    drop(writer);
    drop(export);
}
