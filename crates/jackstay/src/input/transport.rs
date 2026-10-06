//! Versioned, bounded input over a host-authorized local connection: a
//! connected Unix stream, or a named pipe on Windows ([`crate::local`]).
//! Listener selection and authorization are deliberately outside this module.
use std::{
    collections::VecDeque,
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use super::*;
use crate::local::Stream;
/// Exact input protocol version; older peers cannot carry scroll lifecycle data.
pub const VERSION: u32 = 2;
const MAX_FRAME: usize = 128 * 1024;
use crate::framing::Framed;
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
    pub fn start(target: Target, stream: Stream) -> io::Result<Self> {
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
            Some(Wire::Hello { version: VERSION, mode }) => match target.admit(mode) {
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
        version: VERSION,
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
        if closing && wire.idle()? {
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
    pub fn connect(stream: Stream, mode: Mode) -> Result<Self, ConnectError> {
        let mut wire = Framed::new(stream)?;
        wire.send(Wire::Hello { version: VERSION, mode })?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let welcome = loop {
            wire.flush()?;
            match wire.receive()? {
                Some(Wire::Welcome(w)) if w.version == VERSION && w.mode == mode => {
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
                    // Keep target-side count aggregation intact across worker ticks
                    // when the presenter polls more slowly than the connection.
                    if s.status.back_mut().is_some_and(|pending| pending.merge_coalesced(&status)) {
                        continue;
                    }
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

    #[cfg(unix)]
    fn pair() -> (Stream, Stream) {
        Stream::pair().unwrap()
    }

    #[cfg(windows)]
    fn pair() -> (Stream, Stream) {
        crate::local::pipe_pair().unwrap()
    }

    #[cfg(unix)]
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
        let (stream, peer) = pair();
        let mut wire = Framed::new(stream).unwrap();
        drop(peer);
        wire.send(Wire::Reset).unwrap();
        assert_eq!(wire.flush().unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn close_ack_is_read_when_heartbeat_is_due_and_peer_has_closed() {
        let (a, b) = pair();
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
                version: VERSION,
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
    fn wait<T>(mut f: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(v) = f() {
                return v;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(STEP);
        }
    }
    // A v1 Hello must never admit a controller or dispatch work on a v2 target.
    #[test]
    fn wire_v1_hello_is_refused_before_admission() {
        let target = Target::new(Config::default()).unwrap();
        let (a, b) = pair();
        let server = Server::start(target.clone(), a).unwrap();
        let mut peer = Framed::new(b).unwrap();
        peer.send(Wire::Hello {
            version: 1,
            mode: Mode::Cooperative,
        })
        .unwrap();
        peer.flush().unwrap();
        wait(|| server.finished().then_some(()));
        assert!(target.idle());
        assert!(target.next().is_none());
    }
    // A structurally valid v1 Welcome cannot create a v2 client, even with a
    // valid configuration; neither side silently discards lifecycle metadata.
    #[test]
    fn wire_v1_welcome_is_refused_before_client_creation() {
        let (a, b) = pair();
        let peer = thread::spawn(move || {
            let mut wire = Framed::new(a).unwrap();
            let hello: Wire = wait(|| wire.receive().unwrap());
            assert!(matches!(hello, Wire::Hello { version: VERSION, .. }));
            wire.send(Wire::Welcome(Welcome {
                version: 1,
                mode: Mode::Cooperative,
                controller: 1,
                epoch: 1,
                config: Config::default(),
            }))
            .unwrap();
            wire.flush().unwrap();
        });
        assert!(Client::connect(b, Mode::Cooperative).is_err());
        peer.join().unwrap();
    }
    // Wire shape validation accepts Stationary, but semantic target admission
    // rejects nonzero deltas without dispatch or cleanup and retains the gate.
    #[test]
    fn structurally_valid_stationary_wire_sample_is_semantically_rejected() {
        let target = Target::new(Config::default()).unwrap();
        let (a, b) = pair();
        let _server = Server::start(target.clone(), a).unwrap();
        let mut peer = Framed::new(b).unwrap();
        peer.send(Wire::Hello {
            version: VERSION,
            mode: Mode::Cooperative,
        })
        .unwrap();
        peer.flush().unwrap();
        assert!(matches!(wait(|| peer.receive::<Wire>().unwrap()), Wire::Welcome(_)));
        let event = |phase, x| Event::Scroll {
            x,
            y: 0.0,
            unit: ScrollUnit::Pixel,
            position: Position {
                revision: 1,
                x: 1.0,
                y: 1.0,
            },
            phase: Some(phase),
            momentum_phase: Some(MomentumPhase::None),
            inverted_from_device: Some(false),
        };
        peer.send(Wire::Submit {
            epoch: 1,
            sequence: 1,
            event: event(ScrollPhase::Began, 0.0),
        })
        .unwrap();
        peer.flush().unwrap();
        let work = wait(|| target.next());
        target.complete(work.id, Outcome::Executed).unwrap();
        assert!(matches!(
            wait(|| peer.receive::<Wire>().unwrap()),
            Wire::Reply(Status::Completed { sequence: 1, .. })
        ));
        peer.send(Wire::Submit {
            epoch: 1,
            sequence: 2,
            event: event(ScrollPhase::Stationary, 1.0),
        })
        .unwrap();
        peer.flush().unwrap();
        assert!(matches!(
            wait(|| peer.receive::<Wire>().unwrap()),
            Wire::Reply(Status::Rejected {
                sequence: 2,
                error: Error::Invalid
            })
        ));
        assert!(target.next().is_none());
        peer.send(Wire::Submit {
            epoch: 1,
            sequence: 3,
            event: event(ScrollPhase::Changed, 1.0),
        })
        .unwrap();
        peer.flush().unwrap();
        let work = wait(|| target.next());
        assert_eq!(work.operation, Operation::Event(event(ScrollPhase::Changed, 1.0)));
        target.complete(work.id, Outcome::Executed).unwrap();
    }
}
