//! Rust producer for scripts/smoke-swift.sh. Only the payload fd is inherited
//! by the Swift child; JSON carries the public writer descriptor and slot.

#[cfg(unix)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::{
        io::Write,
        os::{fd::AsRawFd, unix::process::CommandExt},
        process::{Command, Stdio},
        time::Duration,
    };

    use jackstay::acquisition::arena::{ArenaConfig, ArenaProducer};

    let executable = std::env::args_os().nth(1).ok_or("expected Swift executable path")?;
    let mut producer = ArenaProducer::new(ArenaConfig {
        resource_capacity: 6,
        retained_history: 2,
        producer_reserve: 1,
        payload_capacity: 4,
        memory_budget: 1 << 20,
        max_incarnations: 2,
        drain_timeout: Duration::from_secs(5),
    })?;
    let mut reservation = producer.reserve()?.ok_or("no CPU slot available")?;
    let export = producer.export_writer()?.ok_or("no writer export available")?;
    // SAFETY: export stays alive until the child exits and all object copies
    // close. Only the child writes the reserved slot, before the parent reads.
    let object = unsafe { export.duplicate_object()? };
    let fd = object.as_raw_fd();
    let message = serde_json::to_vec(&serde_json::json!({
        "descriptor": export.descriptor(),
        "slot": reservation.slot(),
        "object": fd,
    }))?;
    let mut command = Command::new(executable);
    command.stdin(Stdio::piped());
    // SAFETY: pre_exec performs only an async-signal-safe fcntl call on a live
    // fd. Clear CLOEXEC in the child only; the parent retains its owned copy.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let write_result = child.stdin.take().ok_or("missing child stdin")?.write_all(&message);
    let status = child.wait()?;
    // Process exit closes any child mapping/object, including on test failure.
    drop(object);
    write_result?;
    assert!(status.success(), "Swift writer failed: {status}");
    assert_eq!(reservation.bytes_mut(), b"Swif");
    drop(export);
    producer.abandon(reservation)?;
    println!("Swift delegated writer: Rust verified shared payload");
    Ok(())
}

#[cfg(not(unix))]
fn main() {
    panic!("the Swift writer smoke requires Unix fd inheritance");
}
