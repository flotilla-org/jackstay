//! Versioned, bounded input over a host-authorized connected Unix stream.
//! Listener selection and authorization are deliberately outside this module.
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use super::*;
const MAX_FRAME: usize = 128 * 1024;
const MAX_BUFFER: usize = 512 * 1024;
const STEP: Duration = Duration::from_millis(5);
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Welcome {
    pub mode: Mode,
    pub version: u32,
    pub controller: u64,
    pub epoch: u64,
    pub config: Config,
}
#[derive(Serialize, Deserialize)]
enum Wire {
    Hello { version: u32, mode: Mode },
    Welcome(Welcome),
    Submit { epoch: u64, sequence: u64, event: Event },
    Reset,
    Heartbeat,
    Reply(Status),
    Reject(Error),
    Close,
}
struct Framed {
    stream: UnixStream,
    input: Vec<u8>,
    output: VecDeque<Vec<u8>>,
    offset: usize,
    queued: usize,
}
impl Framed {
    fn new(stream: UnixStream) -> io::Result<Self> {
        crate::socket_options::suppress_sigpipe(&stream)?;
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            input: Vec::new(),
            output: VecDeque::new(),
            offset: 0,
            queued: 0,
        })
    }
    fn send(&mut self, msg: Wire) -> io::Result<()> {
        let json = serde_json::to_vec(&msg)?;
        if json.len() > MAX_FRAME || self.queued + json.len() + 4 > MAX_BUFFER {
            return Err(io::Error::other("input wire bound exceeded"));
        }
        let mut frame = Vec::with_capacity(json.len() + 4);
        frame.extend_from_slice(&(json.len() as u32).to_be_bytes());
        frame.extend(json);
        self.queued += frame.len();
        self.output.push_back(frame);
        Ok(())
    }
    fn flush(&mut self) -> io::Result<()> {
        while let Some(frame) = self.output.front() {
            match self.stream.write(&frame[self.offset..]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.offset += n;
                    self.queued -= n;
                    if self.offset == frame.len() {
                        self.output.pop_front();
                        self.offset = 0;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
    fn receive(&mut self) -> io::Result<Option<Wire>> {
        loop {
            if self.input.len() >= 4 {
                let size = u32::from_be_bytes(self.input[..4].try_into().unwrap()) as usize;
                if size == 0 || size > MAX_FRAME {
                    return Err(io::Error::other("invalid input frame size"));
                }
                if self.input.len() >= size + 4 {
                    let msg = serde_json::from_slice(&self.input[4..size + 4])?;
                    self.input.drain(..size + 4);
                    return Ok(Some(msg));
                }
            }
            let mut buf = [0; 4096];
            match self.stream.read(&mut buf) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => self.input.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }
}
/// Owner of the network worker. Dropping it ends the controller and schedules
/// cleanup on Target; the host must continue pumping its executor until idle.
pub struct Server {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Server {
    pub fn finished(&self) -> bool {
        self.worker.as_ref().is_none_or(|w| w.is_finished())
    }
    pub fn start(target: Target, stream: UnixStream) -> io::Result<Self> {
        let wire = Framed::new(stream)?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let worker = thread::Builder::new().name("jackstay-input-server".into()).spawn(move || {
            let _ = serve(target, wire, flag);
        })?;
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
fn serve(target: Target, mut wire: Framed, stop: Arc<AtomicBool>) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let controller = loop {
        if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Ok(());
        }
        match wire.receive()? {
            Some(Wire::Hello { version: 1, mode }) => match target.admit(mode) {
                Ok(c) => break c,
                Err(e) => {
                    wire.send(Wire::Reject(e))?;
                    wire.flush()?;
                    return Ok(());
                }
            },
            Some(_) => return Err(io::Error::other("expected input hello")),
            None => thread::sleep(STEP),
        }
    };
    let config = target.config();
    wire.send(Wire::Welcome(Welcome {
        version: 1,
        mode: controller.mode(),
        controller: controller.id(),
        epoch: 1,
        config: config.clone(),
    }))?;
    let interval = (config.idle_timeout / 4).min(Duration::from_secs(1));
    let mut heartbeat = Instant::now();
    let mut closing = false;
    while !stop.load(Ordering::Acquire) {
        target.tick();
        if !closing {
            for _ in 0..32 {
                let Some(msg) = wire.receive()? else {
                    break;
                };
                match msg {
                    Wire::Submit { epoch, sequence, event } => {
                        if let Err(error) = controller.submit(epoch, sequence, event) {
                            wire.send(Wire::Reply(Status::Rejected { sequence, error }))?;
                        }
                    }
                    Wire::Heartbeat => {
                        let _ = controller.heartbeat();
                    }
                    Wire::Reset => {
                        controller.reset().map_err(|e| io::Error::other(format!("reset: {e:?}")))?;
                    }
                    Wire::Close => controller.close(),
                    _ => return Err(io::Error::other("unexpected input message")),
                }
            }
        }
        while let Some(status) = controller.poll() {
            if matches!(status, Status::Closed { .. }) {
                closing = true;
            }
            wire.send(Wire::Reply(status))?;
        }
        if !closing && heartbeat.elapsed() >= interval {
            wire.send(Wire::Heartbeat)?;
            heartbeat = Instant::now();
        }
        wire.flush()?;
        if closing && wire.output.is_empty() {
            return Ok(());
        }
        thread::sleep(STEP);
    }
    Ok(())
}
struct ClientState {
    welcome: Welcome,
    sequence: u64,
    queue: VecDeque<Wire>,
    bytes: usize,
    status: VecDeque<Status>,
    alive: bool,
    closing: bool,
    resetting: bool,
}
#[derive(Debug)]
pub enum ConnectError {
    Admission(Error),
    Transport(io::Error),
}
impl From<io::Error> for ConnectError {
    fn from(e: io::Error) -> Self {
        Self::Transport(e)
    }
}
impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ConnectError {}
pub struct Client {
    state: Arc<Mutex<ClientState>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Client {
    /// Performs bounded startup (five seconds). Call off an input/render thread.
    pub fn connect(stream: UnixStream, mode: Mode) -> Result<Self, ConnectError> {
        let mut wire = Framed::new(stream)?;
        wire.send(Wire::Hello { version: 1, mode })?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let welcome = loop {
            wire.flush()?;
            match wire.receive()? {
                Some(Wire::Welcome(w)) if w.version == 1 && w.mode == mode => {
                    Target::new(w.config.clone()).map_err(|_| io::Error::other("invalid input offer"))?;
                    break w;
                }
                Some(Wire::Reject(e)) => return Err(ConnectError::Admission(e)),
                Some(_) => return Err(io::Error::other("invalid input welcome").into()),
                None => {}
            }
            if Instant::now() >= deadline {
                return Err(io::Error::from(io::ErrorKind::TimedOut).into());
            }
            thread::sleep(STEP);
        };
        let state = Arc::new(Mutex::new(ClientState {
            welcome,
            sequence: 0,
            queue: VecDeque::new(),
            bytes: 0,
            status: VecDeque::new(),
            alive: true,
            closing: false,
            resetting: false,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let shared = state.clone();
        let worker = thread::Builder::new().name("jackstay-input-client".into()).spawn(move || {
            let _ = drive_client(wire, shared.clone(), flag, Instant::now());
            let mut s = shared.lock().unwrap();
            if s.alive {
                s.status.push_back(Status::Closed {
                    reason: Reason::Disconnect,
                    clean: false,
                });
            }
            s.alive = false;
        })?;
        Ok(Self {
            state,
            stop,
            worker: Some(worker),
        })
    }
    pub fn welcome(&self) -> Welcome {
        self.state.lock().unwrap().welcome.clone()
    }
    pub fn send(&self, event: Event) -> Result<u64, Error> {
        let mut s = self.state.lock().unwrap();
        if !s.alive || s.closing {
            return Err(Error::Closed);
        }
        if s.resetting {
            return Err(Error::Busy);
        }
        super::session::validate(&s.welcome.config, s.welcome.mode, &event)?;
        if s.queue.len() >= s.welcome.config.max_events || s.bytes + event.bytes() > s.welcome.config.max_bytes {
            self.stop.store(true, Ordering::Release);
            s.closing = true;
            return Err(Error::Overflow);
        }
        s.sequence = s.sequence.checked_add(1).ok_or(Error::Closed)?;
        let sequence = s.sequence;
        let epoch = s.welcome.epoch;
        s.bytes += event.bytes();
        s.queue.push_back(Wire::Submit { epoch, sequence, event });
        Ok(sequence)
    }
    pub fn reset(&self) -> Result<(), Error> {
        let mut s = self.state.lock().unwrap();
        if !s.alive || s.closing {
            return Err(Error::Closed);
        }
        s.queue.clear();
        s.bytes = 0;
        s.queue.push_back(Wire::Reset);
        s.resetting = true;
        Ok(())
    }
    pub fn close(&self) {
        let mut s = self.state.lock().unwrap();
        s.closing = true;
        s.queue.clear();
        s.bytes = 0;
        s.queue.push_back(Wire::Close);
    }
    pub fn poll(&self) -> Option<Status> {
        self.state.lock().unwrap().status.pop_front()
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
fn drive_client(mut wire: Framed, state: Arc<Mutex<ClientState>>, stop: Arc<AtomicBool>, mut heartbeat: Instant) -> io::Result<()> {
    let timeout = state.lock().unwrap().welcome.config.idle_timeout;
    let interval = (timeout / 4).min(Duration::from_secs(1));
    let mut last_seen = Instant::now();
    while !stop.load(Ordering::Acquire) {
        let closing = {
            let mut s = state.lock().unwrap();
            // Keep the stream buffer bounded independently of the application queue.
            if wire.queued < MAX_FRAME {
                for _ in 0..32 {
                    let Some(msg) = s.queue.pop_front() else {
                        break;
                    };
                    if let Wire::Submit { event, .. } = &msg {
                        s.bytes -= event.bytes();
                    }
                    wire.send(msg)?;
                    if wire.queued >= MAX_FRAME {
                        break;
                    }
                }
            }
            s.closing
        };
        // Once Close is queued, stop originating heartbeats. The peer may
        // already have sent its final acknowledgement and closed; writing first
        // would turn that clean close into BrokenPipe before we read the reply.
        if !closing && heartbeat.elapsed() >= interval {
            wire.send(Wire::Heartbeat)?;
            heartbeat = Instant::now();
        }
        wire.flush()?;
        for _ in 0..32 {
            let Some(msg) = wire.receive()? else {
                break;
            };
            last_seen = Instant::now();
            match msg {
                Wire::Heartbeat => {}
                Wire::Reply(status) => {
                    let mut s = state.lock().unwrap();
                    if let Status::Reset { epoch, geometry } = &status {
                        s.resetting = false;
                        s.welcome.epoch = *epoch;
                        s.welcome.config.geometry = *geometry;
                    }
                    let closed = matches!(status, Status::Closed { .. });
                    if s.status.len() >= s.welcome.config.max_events * 2 + 4 {
                        return Err(io::Error::other("unconsumed input results"));
                    }
                    s.status.push_back(status);
                    if closed {
                        s.alive = false;
                        return Ok(());
                    }
                }
                _ => return Err(io::Error::other("unexpected input reply")),
            }
        }
        if last_seen.elapsed() >= timeout {
            return Err(io::ErrorKind::TimedOut.into());
        }
        thread::sleep(STEP);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_peer_is_an_error_with_default_sigpipe() {
        const CHILD: &str = "JACKSTAY_TEST_DEFAULT_SIGPIPE";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "input::transport::tests::closed_peer_is_an_error_with_default_sigpipe"])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success(), "C-host signal disposition killed the child: {status}");
            return;
        }
        // Change process-wide state only in this dedicated test subprocess.
        // Rust normally ignores SIGPIPE at startup; C hosts need not do so.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
        let (stream, peer) = UnixStream::pair().unwrap();
        let mut wire = Framed::new(stream).unwrap();
        drop(peer);
        wire.send(Wire::Reset).unwrap();
        assert_eq!(wire.flush().unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn close_ack_is_read_when_heartbeat_is_due_and_peer_has_closed() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut peer = Framed::new(a).unwrap();
        let wire = Framed::new(b).unwrap();
        let closed = Status::Closed {
            reason: Reason::Disconnect,
            clean: true,
        };
        peer.send(Wire::Reply(closed.clone())).unwrap();
        peer.flush().unwrap();
        drop(peer);

        // Resume the real worker after Close was flushed. The peer's final
        // acknowledgement is readable, but any further write gets BrokenPipe.
        let state = Arc::new(Mutex::new(ClientState {
            welcome: Welcome {
                version: 1,
                mode: Mode::Cooperative,
                controller: 1,
                epoch: 1,
                config: Config::default(),
            },
            sequence: 0,
            queue: VecDeque::new(),
            bytes: 0,
            status: VecDeque::new(),
            alive: true,
            closing: true,
            resetting: false,
        }));
        let heartbeat = Instant::now() - Duration::from_secs(2);
        drive_client(wire, state.clone(), Arc::new(AtomicBool::new(false)), heartbeat).unwrap();
        let state = state.lock().unwrap();
        assert!(!state.alive);
        assert_eq!(state.status.iter().cloned().collect::<Vec<_>>(), vec![closed]);
    }
}
