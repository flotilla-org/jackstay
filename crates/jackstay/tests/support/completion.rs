//! Controlled cross-process completion source for acquisition tests. Completion
//! values travel over their own event channel, never the frame/setup broker.
use std::{
    io::{Read, Write},
    os::{fd::OwnedFd, unix::net::UnixStream},
    sync::{Arc, Mutex},
    thread::JoinHandle,
};

use jackstay::{
    Result,
    acquisition::arena::{ReleaseNotification, ReleaseTimeline},
};

#[derive(Debug, Default)]
struct State(Mutex<(u64, Vec<(u64, ReleaseNotification)>)>);

impl State {
    fn signal(&self, value: u64) {
        let mut state = self.0.lock().unwrap();
        assert!(value >= state.0);
        state.0 = value;
        state.1.retain(|(target, wake)| {
            if *target <= value {
                wake.notify().unwrap();
                false
            } else {
                true
            }
        });
    }
}

#[derive(Debug, Default)]
pub struct SharedCompletion {
    state: Arc<State>,
    writer: Mutex<Option<UnixStream>>,
    receiver: Option<(UnixStream, JoinHandle<()>)>,
}

impl SharedCompletion {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn export_fd(&self) -> OwnedFd {
        let mut writer = self.writer.lock().unwrap();
        assert!(writer.is_none(), "one remote observer in these tests");
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        sender.write_all(&self.state.0.lock().unwrap().0.to_le_bytes()).unwrap();
        *writer = Some(sender);
        receiver.into()
    }

    pub fn from_fd(fd: OwnedFd) -> Self {
        let state = Arc::new(State::default());
        let mut receiver = UnixStream::from(fd);
        let shutdown = receiver.try_clone().unwrap();
        let observed = Arc::clone(&state);
        let worker = std::thread::spawn(move || {
            let mut bytes = [0; 8];
            while receiver.read_exact(&mut bytes).is_ok() {
                observed.signal(u64::from_le_bytes(bytes));
            }
        });
        Self {
            state,
            writer: Mutex::new(None),
            receiver: Some((shutdown, worker)),
        }
    }

    pub fn signal(&self, value: u64) {
        let mut writer = self.writer.lock().unwrap();
        self.state.signal(value);
        if let Some(writer) = writer.as_mut() {
            // A crash intentionally destroys the receiver before the parent
            // establishes later external completion for producer cleanup.
            if let Err(error) = writer.write_all(&value.to_le_bytes()) {
                assert!(matches!(
                    error.kind(),
                    std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                ));
            }
        }
    }
}

impl ReleaseTimeline for SharedCompletion {
    fn completed_value(&self) -> Result<u64> {
        Ok(self.state.0.lock().unwrap().0)
    }

    fn notify_at(&self, value: u64, wake: ReleaseNotification) -> Result<()> {
        let mut state = self.state.0.lock().unwrap();
        if state.0 >= value {
            wake.notify().unwrap();
        } else {
            state.1.push((value, wake));
        }
        Ok(())
    }
}

impl Drop for SharedCompletion {
    fn drop(&mut self) {
        if let Some((socket, worker)) = self.receiver.take() {
            let _ = socket.shutdown(std::net::Shutdown::Both);
            worker.join().unwrap();
        }
    }
}
