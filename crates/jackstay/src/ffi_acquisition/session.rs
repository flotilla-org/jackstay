//! Owned CPU setup connections, with optional host session selection.

use std::{
    ffi::{CStr, c_char},
    ptr,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use super::{FtAcquisitionConsumer, status};
use crate::{
    acquisition::{arena::ConfigurationInstall, socket::CpuSetupClient},
    daemon,
    ffi::*,
};

pub struct FtCpuAcquisitionConnection {
    client: Mutex<CpuSetupClient>,
    shutdown: crate::local::ShutdownHandle,
    cancelled: AtomicBool,
}

impl FtCpuAcquisitionConnection {
    // A completed operation keeps its result, even if cancellation arrives
    // before we return it. Cancellation interrupts failures and future calls;
    // it cannot undo an installed configuration or a transferred consumer.
    fn with_client<T>(&self, operation: impl FnOnce(&mut CpuSetupClient) -> Result<T, FtStatus>) -> Result<T, FtStatus> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(FT_STATUS_CANCELLED);
        }
        let mut client = self.client.lock().map_err(|_| FT_STATUS_ERROR)?;
        operation(&mut client).map_err(|error| {
            if self.cancelled.load(Ordering::Acquire) {
                FT_STATUS_CANCELLED
            } else {
                error
            }
        })
    }

    fn new(client: CpuSetupClient) -> std::io::Result<Self> {
        let shutdown = client.shutdown_handle()?;
        Ok(Self {
            client: Mutex::new(client),
            shutdown,
            cancelled: AtomicBool::new(false),
        })
    }
}

/// Own an already connected host-selected/authorized Unix setup stream.
/// No admission I/O happens until attach, so cancellation can be installed first.
///
/// # Safety
/// fd and out are writable/disjoint; fd owns the live stream; *out is null.
/// The peer is the conforming sole producer. This process must be the original
/// peer and sole recipient of grants; no caller FD copies, fork, forwarding or
/// replay is allowed. After basic checks fd is consumed/set to -1 on all outcomes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_cpu_connection_create(fd: *mut i32, out: *mut *mut FtCpuAcquisitionConnection) -> FtStatus {
    // SAFETY: caller supplies valid disjoint writable storage.
    let (Some(fd), Some(out)) = (unsafe { fd.as_mut() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if *fd < 0 || !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: caller transfers sole ownership and grants are bound to this peer.
    let Ok(stream) = (unsafe { super::setup_server::take_stream(fd) }) else {
        return FT_STATUS_ERROR;
    };
    // SAFETY: caller guarantees the trusted producer and sole-recipient contract.
    let client = unsafe { CpuSetupClient::from_stream(stream) };
    match FtCpuAcquisitionConnection::new(client) {
        Ok(connection) => {
            *out = Box::into_raw(Box::new(connection));
            FT_STATUS_OK
        }
        Err(_) => FT_STATUS_ERROR,
    }
}

/// Admit a process-bound consumer. May block on setup I/O; cancel interrupts it.
///
/// # Safety
/// Connection is live, output is writable, disjoint and null. Serialize attach
/// and configuration operations; cancel may run concurrently. Imported maps and
/// frames must never be forked, forwarded or replayed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_cpu_attach(
    connection: *mut FtCpuAcquisitionConnection,
    holding: u32,
    out: *mut *mut FtAcquisitionConsumer,
) -> FtStatus {
    // SAFETY: caller supplies live connection and disjoint writable output.
    let (Some(connection), Some(out)) = (unsafe { connection.as_ref() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if holding == 0 || !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    match connection.with_client(|client| client.attach(holding).map_err(|_| FT_STATUS_ERROR)) {
        Ok(consumer) => {
            *out = FtAcquisitionConsumer::into_raw(consumer);
            FT_STATUS_OK
        }
        Err(status) => status,
    }
}

/// Permanently interrupt this connection's setup I/O. Frames remain owned.
///
/// # Safety
/// Connection is null or live. May run concurrently with attach/configuration,
/// but all calls must return before destroying the connection handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_cpu_connection_cancel(connection: *const FtCpuAcquisitionConnection) {
    // SAFETY: caller keeps the handle alive until this operation returns.
    if let Some(connection) = unsafe { connection.as_ref() } {
        connection.cancelled.store(true, Ordering::Release);
        connection.shutdown.shutdown();
    }
}

/// Select a trusted daemon's session, authorize it and request holding capacity.
///
/// # Safety
/// Strings are live NUL-terminated UTF-8 (token may be null). Outputs are writable,
/// exclusive and do not alias; both handle outputs start null. The daemon must
/// obey the common sole-producer contract. Never fork/forward/replay mappings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_cpu_connect_session(
    control_path: *const c_char,
    session_id: *const c_char,
    token: *const c_char,
    holding: u32,
    out_connection: *mut *mut FtCpuAcquisitionConnection,
    out_consumer: *mut *mut FtAcquisitionConsumer,
    out_track: *mut u64,
) -> FtStatus {
    if control_path.is_null() || session_id.is_null() || holding == 0 {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: output validity and exclusivity are caller obligations.
    let (Some(connection), Some(consumer), Some(track)) = (unsafe { out_connection.as_mut() }, unsafe { out_consumer.as_mut() }, unsafe {
        out_track.as_mut()
    }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !connection.is_null() || !consumer.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    *track = 0;
    // SAFETY: the caller supplies live NUL-terminated strings.
    let (Ok(control_path), Ok(session_id)) = (
        unsafe { CStr::from_ptr(control_path) }.to_str(),
        unsafe { CStr::from_ptr(session_id) }.to_str(),
    ) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let token = if token.is_null() {
        None
    } else {
        // SAFETY: non-null token is a live string for this call.
        let Ok(token) = unsafe { CStr::from_ptr(token) }.to_str() else {
            return FT_STATUS_INVALID_ARGUMENT;
        };
        Some(token.to_owned())
    };
    let Ok(mut info) = daemon::get_session(control_path, session_id) else {
        return FT_STATUS_ERROR;
    };
    info.bearer_token = token;
    // SAFETY: delegated to the caller's trusted daemon / sole-recipient contract.
    let Ok(client) = (unsafe { daemon::open_cpu_setup(&info) }) else {
        return FT_STATUS_ERROR;
    };
    // Allocate the shutdown descriptor before admission can create map owners.
    let Ok(setup) = FtCpuAcquisitionConnection::new(client) else {
        return FT_STATUS_ERROR;
    };
    let admitted = match setup.with_client(|client| client.attach(holding).map_err(|_| FT_STATUS_ERROR)) {
        Ok(consumer) => consumer,
        Err(status) => return status,
    };
    *track = info.track_id;
    *connection = Box::into_raw(Box::new(setup));
    *consumer = FtAcquisitionConsumer::into_raw(admitted);
    FT_STATUS_OK
}

/// Install a pending CPU generation on this connection's existing consumer.
///
/// # Safety
/// Handles belong to the matching setup. Consumer is exclusive. Serialize setup
/// calls; connection cancellation may run concurrently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_cpu_install_configuration(
    connection: *mut FtCpuAcquisitionConnection,
    consumer: *mut FtAcquisitionConsumer,
) -> FtStatus {
    // SAFETY: caller supplies live exclusive handles.
    let (Some(connection), Some(consumer)) = (unsafe { connection.as_ref() }, unsafe { consumer.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    match connection.with_client(|client| {
        client.install_configuration(&mut consumer.0).map_err(|error| match error {
            crate::acquisition::socket::SocketError::Arena(error) => status(error),
            _ => FT_STATUS_ERROR,
        })
    }) {
        Ok(Some(ConfigurationInstall::Installed)) => FT_STATUS_OK,
        Ok(Some(ConfigurationInstall::Stale)) => FT_STATUS_STALE,
        Ok(None) => FT_STATUS_EMPTY,
        Err(status) => status,
    }
}

/// # Safety
/// Pointer is writable and exclusively owns its handle. No concurrent setup call
/// may use it. Consumer and acquired-frame handles retain independent ownership.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_cpu_connection_destroy(connection: *mut *mut FtCpuAcquisitionConnection) {
    // SAFETY: exclusive handle ownership is a caller obligation.
    if let Some(connection) = unsafe { connection.as_mut() }
        && !connection.is_null()
    {
        // SAFETY: this box was produced by this API and is consumed once.
        drop(unsafe { Box::from_raw(std::mem::replace(connection, ptr::null_mut())) });
    }
}

#[cfg(test)]
mod tests {
    use std::{os::unix::net::UnixStream, sync::Arc, thread, time::Duration};

    use super::*;
    use crate::acquisition::{
        arena::{ArenaConfig, ArenaProducer},
        socket::serve_cpu,
    };

    fn completion_race(replace: bool) {
        let producer = Arc::new(Mutex::new(
            ArenaProducer::new(ArenaConfig {
                resource_capacity: 6,
                retained_history: 2,
                producer_reserve: 1,
                max_incarnations: 2,
                payload_capacity: 4,
                memory_budget: 1024 * 1024,
                drain_timeout: Duration::from_secs(5),
            })
            .unwrap(),
        ));
        let (server, client) = UnixStream::pair().unwrap();
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let served = producer.clone();
        let worker = thread::spawn(move || serve_cpu(server, served));
        // SAFETY: the conforming producer and recipient are in this process;
        // no grants or mappings are forwarded or inherited through fork.
        let connection = FtCpuAcquisitionConnection::new(unsafe { CpuSetupClient::from_stream(client) }).unwrap();
        let mut consumer = connection
            .with_client(|client| {
                let result = client.attach(1).map_err(|_| FT_STATUS_ERROR);
                assert!(result.is_ok());
                if !replace {
                    // Place cancellation precisely after real admission completes,
                    // before the connection owner translates the result for C.
                    // SAFETY: shared cancellation borrows a live connection.
                    unsafe { ft_acquisition_cpu_connection_cancel(&connection) };
                }
                result
            })
            .expect("completed admission must transfer its consumer");
        if replace {
            producer.lock().unwrap().reconfigure_cpu(8).unwrap();
            let installed = connection
                .with_client(|client| {
                    let result = client.install_configuration(&mut consumer).map_err(|_| FT_STATUS_ERROR);
                    assert!(matches!(result, Ok(Some(ConfigurationInstall::Installed))));
                    // SAFETY: cancellation does not access the exclusively borrowed
                    // consumer and the connection remains alive through this call.
                    unsafe { ft_acquisition_cpu_connection_cancel(&connection) };
                    result
                })
                .expect("completed replacement must retain its result");
            assert!(matches!(installed, Some(ConfigurationInstall::Installed)));
        }
        assert_eq!(
            connection.with_client::<()>(|_| panic!("cancelled connection must not start more work")),
            Err(FT_STATUS_CANCELLED)
        );
        drop(consumer);
        drop(connection);
        // Local shutdown can surface either EOF or I/O failure to the server.
        let _ = worker.join().unwrap();
        let mut producer = producer.lock().unwrap();
        producer.stop();
        assert!(producer.poll_shutdown_ready().unwrap());
    }

    #[test]
    fn completed_admission_wins_over_late_cancellation() {
        completion_race(false);
    }

    #[test]
    fn completed_replacement_wins_over_late_cancellation() {
        completion_race(true);
    }
}
