//! Relays jackstay input transport connections over the bridge's control
//! connection.
//!
//! The input protocol (`jackstay::input::transport`) is a length-prefixed
//! byte stream over a connected Unix socket with no credentials, descriptor
//! passing or peer checks inside it, so the bridge carries it verbatim. On
//! the consumer host the ingress half accepts controller connections on an
//! owner-only socket; each becomes one relayed stream, opened on the control
//! connection with `Kind::Input` plus `INPUT_OPEN`, fed with the bytes read,
//! and closed with `INPUT_CLOSE` at EOF. On the producer host the egress half
//! answers an open by connecting to the executor's socket and relays the same
//! way back. Both directions share one type.
//!
//! Two rules keep the protocol's invariants intact across the hop. The relay
//! never originates or absorbs a frame: heartbeats are the peers' own proof
//! of liveness, and expiry stays end to end. And a relayed stream is one
//! connection, never transparently reconnected: a new stream is a new
//! controller incarnation whose cleanup the executor sees. Backpressure is
//! the blocking write on the control connection; a local peer that stops
//! reading is cut after a bounded wait rather than buffered.

use std::{
    collections::HashMap,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

use crate::wire::{Kind, Message, flags};

/// Bytes read from a local stream per relayed message.
const CHUNK: usize = 16 * 1024;
/// How long a local peer may refuse bytes before its stream is cut.
const LOCAL_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Both halves' relay: local streams keyed by stream id, and the control
/// connection's write side, shared with the half's own control traffic.
pub struct InputRelay {
    outbound: Arc<Mutex<UnixStream>>,
    streams: Mutex<HashMap<u32, UnixStream>>,
    seq: AtomicU64,
    next_id: AtomicU32,
    /// Where an `INPUT_OPEN` from the peer connects to; `None` on the side
    /// that opens streams itself.
    target: Option<PathBuf>,
    stop: Arc<AtomicBool>,
    pub opened: AtomicU64,
    pub closed: AtomicU64,
    pub bytes_out: AtomicU64,
    pub bytes_in: AtomicU64,
}

impl InputRelay {
    /// `outbound` is the control connection's writer. `target`, when given,
    /// is the local socket an open from the peer is connected to.
    pub fn new(outbound: Arc<Mutex<UnixStream>>, target: Option<PathBuf>, stop: Arc<AtomicBool>) -> Arc<Self> {
        Arc::new(Self {
            outbound,
            streams: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(1 << 32),
            next_id: AtomicU32::new(1),
            target,
            stop,
            opened: AtomicU64::new(0),
            closed: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
            bytes_in: AtomicU64::new(0),
        })
    }

    /// Relays a locally accepted connection as a new stream. The open is
    /// written before the pump starts, so the peer always registers the
    /// stream before any data frame can arrive for it: the pump would
    /// otherwise race the open for the control writer and its first bytes
    /// (the controller's hello) would be dropped for an unknown stream.
    pub fn attach(self: &Arc<Self>, stream: UnixStream) -> std::io::Result<u32> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let reader = self.insert(id, stream)?;
        self.send(id, flags::INPUT_OPEN, Vec::new())?;
        self.spawn_pump(id, reader);
        Ok(id)
    }

    /// Handles an `Input` message from the peer.
    pub fn on_message(self: &Arc<Self>, message: &Message) {
        let id = message.header.stream_id;
        if message.header.flags & flags::INPUT_OPEN != 0 {
            let Some(target) = &self.target else {
                let _ = self.send(id, flags::INPUT_CLOSE, Vec::new());
                return;
            };
            // The peer already knows this stream (its open is what we are
            // handling), so the pump may start at once.
            match UnixStream::connect(target)
                .and_then(|s| self.insert(id, s))
                .map(|reader| self.spawn_pump(id, reader))
            {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("input relay: open of stream {id} to {} failed: {e}", target.display());
                    let _ = self.send(id, flags::INPUT_CLOSE, Vec::new());
                    return;
                }
            }
        }
        if !message.body.is_empty() {
            self.bytes_in.fetch_add(message.body.len() as u64, Ordering::Relaxed);
            let failed = {
                let mut streams = self.streams.lock().expect("relay streams poisoned");
                match streams.get_mut(&id) {
                    Some(stream) => stream.write_all(&message.body).is_err(),
                    None => false,
                }
            };
            if failed {
                eprintln!("input relay: local peer of stream {id} stopped reading; cutting it");
                self.close_local(id);
                let _ = self.send(id, flags::INPUT_CLOSE, Vec::new());
            }
        }
        if message.header.flags & flags::INPUT_CLOSE != 0 {
            self.close_local(id);
        }
    }

    /// Whether the relay currently carries `id`.
    #[must_use]
    pub fn is_open(&self, id: u32) -> bool {
        self.streams.lock().expect("relay streams poisoned").contains_key(&id)
    }

    /// Shuts every local stream; pump threads then end on their own.
    pub fn shutdown(&self) {
        let ids: Vec<u32> = self.streams.lock().expect("relay streams poisoned").keys().copied().collect();
        for id in ids {
            self.close_local(id);
        }
    }

    /// Records `stream` under `id` and returns a reader clone for its pump.
    /// The pump is started separately so the caller can order the open first.
    fn insert(&self, id: u32, stream: UnixStream) -> std::io::Result<UnixStream> {
        // macOS accepted sockets inherit the listener's O_NONBLOCK; the pump
        // reads blocking and must not read a spurious WouldBlock as EOF.
        stream.set_nonblocking(false)?;
        stream.set_write_timeout(Some(LOCAL_WRITE_TIMEOUT))?;
        let reader = stream.try_clone()?;
        self.streams.lock().expect("relay streams poisoned").insert(id, stream);
        self.opened.fetch_add(1, Ordering::Relaxed);
        Ok(reader)
    }

    fn spawn_pump(self: &Arc<Self>, id: u32, reader: UnixStream) {
        let relay = self.clone();
        // If the pump thread cannot start, the stream simply carries no bytes;
        // the connection is still closed on shutdown.
        if let Err(e) = std::thread::Builder::new()
            .name(format!("jackstay-input-relay-{id}"))
            .spawn(move || relay.pump(id, reader))
        {
            eprintln!("input relay: pump for stream {id} did not start: {e}");
        }
    }

    fn pump(self: Arc<Self>, id: u32, mut reader: UnixStream) {
        let mut buf = vec![0u8; CHUNK];
        while !self.stop.load(Ordering::Relaxed) {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    self.bytes_out.fetch_add(n as u64, Ordering::Relaxed);
                    if self.send(id, 0, buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        // EOF or failure on the local side: tell the peer and forget the stream.
        if self.streams.lock().expect("relay streams poisoned").remove(&id).is_some() {
            self.closed.fetch_add(1, Ordering::Relaxed);
            let _ = self.send(id, flags::INPUT_CLOSE, Vec::new());
        }
    }

    fn close_local(&self, id: u32) {
        if let Some(stream) = self.streams.lock().expect("relay streams poisoned").remove(&id) {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            self.closed.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn send(&self, id: u32, stream_flags: u16, body: Vec<u8>) -> std::io::Result<()> {
        let mut message = Message::new(Kind::Input, self.seq.fetch_add(1, Ordering::Relaxed), body);
        message.header.stream_id = id;
        message.header.flags = stream_flags;
        let mut out = self.outbound.lock().expect("control writer poisoned");
        message.write_to(&mut *out).map_err(|e| std::io::Error::other(e.to_string()))
    }
}

/// Binds `path` for controller connections: fresh, owner-only, unlinked on
/// drop. Accepting runs on its own thread and hands each connection to the
/// relay.
pub struct InputListener {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl InputListener {
    pub fn bind(path: &Path, relay: Arc<InputRelay>) -> std::io::Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        if path.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("{}: path exists; another publication may own it", path.display()),
            ));
        }
        let listener = std::os::unix::net::UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new().name("jackstay-input-accept".into()).spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if let Err(e) = relay.attach(stream) {
                                eprintln!("input relay: attach failed: {e}");
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(20)),
                        Err(e) => {
                            eprintln!("input relay: accept: {e}");
                            break;
                        }
                    }
                }
            })?
        };
        Ok(Self {
            path: path.to_path_buf(),
            stop,
            thread: Some(thread),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for InputListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two relays joined by a socket pair standing in for the control link:
    /// bytes pass both ways on an opened stream, and an EOF on one end closes
    /// the other.
    #[test]
    fn bytes_and_eof_cross_the_link() {
        let stop = Arc::new(AtomicBool::new(false));
        let (link_a, link_b) = UnixStream::pair().unwrap();
        let dir = std::env::temp_dir().join(format!("jsb-relay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let executor_path = dir.join("x");
        let executor = std::os::unix::net::UnixListener::bind(&executor_path).unwrap();

        let consumer = InputRelay::new(Arc::new(Mutex::new(link_a.try_clone().unwrap())), None, stop.clone());
        let producer = InputRelay::new(
            Arc::new(Mutex::new(link_b.try_clone().unwrap())),
            Some(executor_path.clone()),
            stop.clone(),
        );
        // Each side reads its end of the link on a thread and feeds its own
        // relay, as the halves' control loops do: what the producer writes to
        // link_b arrives on link_a for the consumer, and the other way round.
        let _readers = [(link_a, consumer.clone()), (link_b, producer.clone())].map(|(mut link, relay)| {
            std::thread::spawn(move || {
                while let Ok(Some(m)) = Message::read_from(&mut link) {
                    relay.on_message(&m);
                }
            })
        });

        let (mut controller, controller_far) = UnixStream::pair().unwrap();
        let id = consumer.attach(controller_far).unwrap();
        let (mut server, _) = executor.accept().unwrap();
        controller.write_all(b"hello executor").unwrap();
        let mut buf = [0u8; 14];
        server.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hello executor");
        server.write_all(b"welcome").unwrap();
        let mut back = [0u8; 7];
        controller.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"welcome");

        drop(controller);
        let mut rest = Vec::new();
        server.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
        for _ in 0..100 {
            if !producer.is_open(id) && !consumer.is_open(id) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!producer.is_open(id));
        assert!(!consumer.is_open(id));
        stop.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The listener accepts real (macOS-nonblocking-inheriting) sockets and
    /// the pump reads them without treating an idle read as EOF: bytes pass
    /// both ways. This is the case the socket-pair tests miss.
    #[test]
    fn a_listener_accepted_stream_relays_without_a_spurious_close() {
        let stop = Arc::new(AtomicBool::new(false));
        let (link_a, link_b) = UnixStream::pair().unwrap();
        let dir = std::env::temp_dir().join(format!("jsb-relay-listen-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let executor_path = dir.join("x");
        let executor = std::os::unix::net::UnixListener::bind(&executor_path).unwrap();
        let consumer = InputRelay::new(Arc::new(Mutex::new(link_a.try_clone().unwrap())), None, stop.clone());
        let producer = InputRelay::new(
            Arc::new(Mutex::new(link_b.try_clone().unwrap())),
            Some(executor_path.clone()),
            stop.clone(),
        );
        let _readers = [(link_a, consumer.clone()), (link_b, producer.clone())].map(|(mut link, relay)| {
            std::thread::spawn(move || {
                while let Ok(Some(m)) = Message::read_from(&mut link) {
                    relay.on_message(&m);
                }
            })
        });
        let listen_path = dir.join("i");
        let _listener = InputListener::bind(&listen_path, consumer.clone()).unwrap();
        let mut controller = UnixStream::connect(&listen_path).unwrap();
        let (mut server, _) = executor.accept().unwrap();
        controller.write_all(b"hello executor").unwrap();
        let mut buf = [0u8; 14];
        server.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hello executor");
        server.write_all(b"welcome back").unwrap();
        let mut back = [0u8; 12];
        controller.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"welcome back");
        stop.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The real input protocol through the two relays: hello and welcome,
    /// an event executed and acknowledged, and the controller's close seen
    /// by the executor as a cleanup it completes.
    #[test]
    fn the_input_protocol_survives_the_relay() {
        use jackstay::input::{
            Config, Event, Mode, Operation, Outcome, Position, Status, Target,
            transport::{Client, Server},
        };
        let stop = Arc::new(AtomicBool::new(false));
        let (link_a, link_b) = UnixStream::pair().unwrap();
        let dir = std::env::temp_dir().join(format!("jsb-relay-proto-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let executor_path = dir.join("x");
        let executor_listener = std::os::unix::net::UnixListener::bind(&executor_path).unwrap();
        let consumer = InputRelay::new(Arc::new(Mutex::new(link_a.try_clone().unwrap())), None, stop.clone());
        let producer = InputRelay::new(
            Arc::new(Mutex::new(link_b.try_clone().unwrap())),
            Some(executor_path.clone()),
            stop.clone(),
        );
        let _readers = [(link_a, consumer.clone()), (link_b, producer.clone())].map(|(mut link, relay)| {
            std::thread::spawn(move || {
                while let Ok(Some(m)) = Message::read_from(&mut link) {
                    relay.on_message(&m);
                }
            })
        });

        // Executor side: a target served on whatever the producer relay connects.
        let target = Target::new(Config::default()).unwrap();
        let executor_target = target.clone();
        let executor = std::thread::spawn(move || {
            let (stream, _) = executor_listener.accept().unwrap();
            let server = Server::start(executor_target.clone(), stream).unwrap();
            let mut events = 0;
            let mut cleanups = 0;
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                match executor_target.next() {
                    Some(work) => {
                        match work.operation {
                            Operation::Event(_) => events += 1,
                            Operation::Cleanup { .. } => cleanups += 1,
                        }
                        executor_target.complete(work.id, Outcome::Executed).unwrap();
                    }
                    None if server.finished() && executor_target.idle() && cleanups > 0 => break,
                    None => std::thread::sleep(Duration::from_millis(2)),
                }
            }
            (events, cleanups)
        });

        // Controller side, as the SDL viewer would connect.
        let (controller_near, controller_far) = UnixStream::pair().unwrap();
        consumer.attach(controller_far).unwrap();
        let client = Client::connect(controller_near, Mode::Cooperative).unwrap();
        let sequence = client
            .send(Event::Motion(Position {
                revision: 1,
                x: 10.0,
                y: 20.0,
            }))
            .unwrap();
        let start = std::time::Instant::now();
        let status = loop {
            if let Some(s) = client.poll() {
                break s;
            }
            assert!(start.elapsed() < Duration::from_secs(3), "no completion through the relay");
            std::thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(
            status,
            Status::Completed {
                sequence,
                outcome: Outcome::Executed
            }
        );
        client.close();
        let (events, cleanups) = executor.join().unwrap();
        assert_eq!((events, cleanups), (1, 1));
        stop.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
