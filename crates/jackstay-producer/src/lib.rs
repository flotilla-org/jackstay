//! Convenience scaffolding over Jackstay's Rust API. No rendering or desktop authority.
use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use jackstay::{
    acquisition::{
        arena::{ArenaConfig, ArenaProducer, FrameDescriptor, ReconfigurationStatus},
        socket::serve_cpu,
    },
    affordances::{Event, Snapshot},
    bootstrap,
    input::{Config, Outcome, Target, Work},
    local::{Endpoint, Listener, ShutdownHandle},
};

/// One owned CPU frame. The callback may return None while content is unchanged.
pub struct Frame {
    pub descriptor: FrameDescriptor,
    pub bytes: Vec<u8>,
}
/// Callbacks are serialized on the source thread. Input completion must reflect
/// actual execution; Cleanup work must release held input before Executed.
/// Verbs are gated by published capabilities. URL policy belongs to the callback.
pub trait Producer: Send + 'static {
    fn frame(&mut self) -> Option<Frame>;
    fn execute(&mut self, work: Work) -> Outcome;
    /// Return consumed storage after publication or a capacity-paused discard.
    /// The toolkit no longer borrows these bytes when this callback runs.
    fn recycle(&mut self, _frame: Frame) {}
    /// Logical input dimensions for a replacement pixel buffer. Scaled renderers
    /// can keep input coordinates independent of the capture's device pixels.
    fn input_size(&mut self, width: u32, height: u32) -> (f64, f64) {
        (f64::from(width), f64::from(height))
    }
    /// Return only changed domains; an empty Vec requires no allocation.
    fn snapshots(&mut self) -> Vec<Snapshot> {
        Vec::new()
    }
    fn affordance(&mut self, _event: Event) {}
}
/// One builder, one running owner. Callbacks must return promptly; stop joins
/// them and will wait for live media leases to retire rather than reuse storage.
pub struct Builder<P> {
    endpoint: Endpoint,
    arena: ArenaConfig,
    input: Config,
    producer: P,
    max_connections: usize,
}
impl<P: Producer> Builder<P> {
    pub fn new(endpoint: Endpoint, arena: ArenaConfig, input: Config, producer: P) -> Self {
        Self {
            endpoint,
            arena,
            input,
            producer,
            max_connections: 8,
        }
    }
    /// Bounds simultaneous bootstrap/media workers, including stalled peers.
    pub fn max_connections(mut self, n: usize) -> Self {
        self.max_connections = n;
        self
    }
    pub fn start(self) -> io::Result<Source> {
        if self.max_connections == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "zero connection limit"));
        }
        let initial_capacity = self.arena.payload_capacity;
        let arena = Arc::new(Mutex::new(ArenaProducer::new(self.arena).map_err(io::Error::other)?));
        let target = Target::new(self.input).map_err(|e| io::Error::other(format!("{e:?}")))?;
        let listener = Arc::new(Listener::bind(&self.endpoint).map_err(io::Error::other)?);
        let stop = Arc::new(AtomicBool::new(false));
        let workers = Arc::new(Mutex::new(Vec::<Peer>::new()));
        let peers = workers.clone();
        let flag = stop.clone();
        let accept_listener = listener.clone();
        let a = arena.clone();
        let t = target.clone();
        let limit = self.max_connections;
        let accept = thread::Builder::new().name("jackstay-source-accept".into()).spawn(move || {
            while !flag.load(Ordering::Acquire) {
                let c = match accept_listener.accept() {
                    Ok(c) => c,
                    Err(jackstay::local::Error::Cancelled) => break,
                    Err(_) => {
                        // Retry persistent accept errors at a fixed, bounded pace.
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                };
                // Internal owner poisoning needs the recovery policy tracked in #58.
                let mut peers = peers.lock().unwrap();
                peers.retain_mut(Peer::alive);
                if flag.load(Ordering::Acquire) {
                    break;
                }
                if peers.len() >= limit {
                    // Silently disconnect rather than admit another worker.
                    continue;
                }
                let stream = c.into_stream();
                let shutdown = match jackstay::local::shutdown_handle(&stream) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let a = a.clone();
                let t = t.clone();
                let channels = Arc::new(Mutex::new(None));
                let shared = channels.clone();
                if let Ok(worker) = thread::Builder::new().name("jackstay-source-peer".into()).spawn(move || {
                    if let Ok(accepted) = bootstrap::accept_v2(stream, Some(t), true) {
                        *shared.lock().unwrap() = Some(Channels {
                            input: accepted.input,
                            affordances: accepted.affordances,
                            sent: std::collections::BTreeMap::new(),
                        });
                        let _ = serve_cpu(accepted.media, a);
                    }
                }) {
                    peers.push(Peer {
                        shutdown,
                        worker: Some(worker),
                        channels,
                    });
                }
            }
        })?;
        let flag = stop.clone();
        let peers = workers.clone();
        let pump_listener = listener.clone();
        let worker = match thread::Builder::new()
            .name("jackstay-source-pump".into())
            .spawn(move || pump(self.producer, arena, target, peers, flag, pump_listener, initial_capacity))
        {
            Ok(w) => w,
            Err(e) => {
                stop.store(true, Ordering::Release);
                listener.cancel();
                let _ = accept.join();
                return Err(e);
            }
        };
        Ok(Source {
            listener,
            stop,
            accept: Some(accept),
            worker: Some(worker),
        })
    }
}
struct Channels {
    input: Option<jackstay::input::transport::Server>,
    affordances: Option<jackstay::affordances::Producer>,
    sent: std::collections::BTreeMap<jackstay::affordances::Domain, Snapshot>,
}
struct Peer {
    shutdown: ShutdownHandle,
    worker: Option<JoinHandle<()>>,
    channels: Arc<Mutex<Option<Channels>>>,
}
impl Peer {
    fn alive(&mut self) -> bool {
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            let _ = self.worker.take().unwrap().join();
        }
        if self.worker.is_some() {
            return true;
        }
        // Media EOF leaves independent channel owners alive and counted.
        self.channels
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|c| c.input.as_ref().is_some_and(|i| !i.finished()) || c.affordances.as_ref().is_some_and(|a| !a.finished()))
    }
}
fn callback<T>(f: impl FnOnce() -> T) -> io::Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).map_err(|_| io::Error::other("producer callback panicked"))
}
/// Running source. Drop performs the same ordered shutdown as stop.
pub struct Source {
    listener: Arc<Listener>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
    worker: Option<JoinHandle<io::Result<()>>>,
}
impl Source {
    /// Whether the pump has ended, after its ordered teardown. Call stop to
    /// retrieve completion/failure; this accessor grants no cleanup authority.
    pub fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }
    pub fn stop(mut self) -> io::Result<()> {
        self.shutdown()
    }
    fn shutdown(&mut self) -> io::Result<()> {
        self.stop.store(true, Ordering::Release);
        self.listener.cancel();
        // Evaluate both joins before reporting either error, including in Drop.
        let accept = if let Some(w) = self.accept.take() {
            w.join().map_err(|_| io::Error::other("accept worker panicked"))
        } else {
            Ok(())
        };
        let pump = if let Some(w) = self.worker.take() {
            w.join().unwrap_or_else(|_| Err(io::Error::other("producer callback panicked")))
        } else {
            Ok(())
        };
        accept.and(pump)
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
fn pump<P: Producer>(
    mut p: P,
    arena: Arc<Mutex<ArenaProducer>>,
    target: Target,
    peers: Arc<Mutex<Vec<Peer>>>,
    stop: Arc<AtomicBool>,
    listener: Arc<Listener>,
    mut capacity: usize,
) -> io::Result<()> {
    let mut snapshots = std::collections::BTreeMap::new();
    let mut size = None;
    let mut pending = None;
    let mut error = None;
    while !stop.load(Ordering::Acquire) {
        target.tick();
        while let Some(work) = target.next() {
            let id = work.id;
            let outcome = callback(|| p.execute(work)).unwrap_or_else(|e| {
                error = Some(e);
                Outcome::Uncertain
            });
            if let Err(e) = target.complete(id, outcome) {
                error = Some(io::Error::other(format!("input completion: {e:?}")));
                break;
            }
        }
        if error.is_some() {
            break;
        }
        let changed = match callback(|| p.snapshots()) {
            Ok(v) => v,
            Err(e) => {
                error = Some(e);
                break;
            }
        };
        for snapshot in changed {
            snapshots.insert(snapshot.domain(), snapshot);
        }
        let mut events = Vec::new();
        {
            let peers = peers.lock().unwrap();
            for peer in peers.iter() {
                let mut channels = peer.channels.lock().unwrap();
                if let Some(c) = channels.as_mut() {
                    if let Some(a) = c.affordances.as_ref() {
                        for (domain, snapshot) in &snapshots {
                            if c.sent.get(domain) != Some(snapshot) && a.publish(snapshot.clone()).is_ok() {
                                c.sent.insert(*domain, snapshot.clone());
                            }
                        }
                        while let Some(e) = a.poll() {
                            events.push(e)
                        }
                    }
                }
            }
        }
        for event in events {
            if let Err(e) = callback(|| p.affordance(event)) {
                error = Some(e);
                break;
            }
        }
        if error.is_some() {
            break;
        }
        let frame = match callback(|| p.frame()) {
            Ok(v) => v,
            Err(e) => {
                error = Some(e);
                break;
            }
        };
        if let Some(frame) = frame {
            let result = callback(|| {
                let dims = (frame.descriptor.width, frame.descriptor.height, frame.descriptor.stride);
                let mut a = arena.lock().unwrap();
                // Avoid replacing the caller's correctly sized initial arena: a
                // consumer may attach before the first callback supplies its frame.
                if size.is_none() && pending.is_none() && frame.bytes.len() <= capacity {
                    size = Some(dims);
                }
                if pending.is_some() || size != Some(dims) || frame.bytes.len() > capacity {
                    let status = if pending.is_some() {
                        a.advance_reconfiguration()
                    } else {
                        pending = Some((dims, frame.bytes.len()));
                        a.reconfigure_cpu(frame.bytes.len())
                    };
                    match status {
                        Ok(ReconfigurationStatus::Ready { .. }) => {
                            let (installed, bytes) = pending.take().unwrap();
                            if size.is_some_and(|old| old != installed) {
                                let old = target.config().geometry;
                                let (width, height) = p.input_size(installed.0, installed.1);
                                let geometry = jackstay::input::Geometry {
                                    revision: old.revision.saturating_add(1),
                                    width,
                                    height,
                                };
                                if old.width != width || old.height != height {
                                    if let Err(e) = target.set_geometry(geometry) {
                                        return Err(io::Error::other(format!("geometry: {e:?}")));
                                    }
                                }
                            }
                            size = Some(installed);
                            capacity = bytes;
                            if installed != dims || frame.bytes.len() > capacity {
                                drop(a);
                                thread::sleep(Duration::from_millis(5));
                                return Ok(());
                            }
                        }
                        Ok(ReconfigurationStatus::PausedCapacity { .. }) => {
                            drop(a);
                            thread::sleep(Duration::from_millis(5));
                            return Ok(());
                        }
                        Err(e) => {
                            return Err(io::Error::other(e));
                        }
                    }
                }
                a.publish(frame.descriptor, &frame.bytes).map(|_| ()).map_err(io::Error::other)
            })
            .and_then(|result| result);
            let recycled = callback(|| p.recycle(frame));
            if let Err(e) = result.and(recycled) {
                error = Some(e);
                break;
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    // Caller callback panics are isolated above. Recovery from a library-internal
    // invariant panic/poisoned owner needs a separate ownership policy (#58):
    // unlocking inconsistent arena/Target state is not proof of clean release.
    // Cancel accepts, interrupt setup/media, join workers, then drop input
    // servers before draining the executor and freeing its target.
    stop.store(true, Ordering::Release);
    listener.cancel();
    let mut peers = peers.lock().unwrap();
    for peer in peers.iter() {
        peer.shutdown.shutdown();
    }
    for peer in peers.iter_mut() {
        if let Some(w) = peer.worker.take() {
            let _ = w.join();
        }
        if let Some(mut c) = peer.channels.lock().unwrap().take() {
            drop(c.input.take());
            drop(c.affordances.take());
        }
    }
    drop(peers);
    while let Some(work) = target.next() {
        let id = work.id;
        let outcome = callback(|| p.execute(work)).unwrap_or_else(|e| {
            error = Some(e);
            Outcome::Uncertain
        });
        let _ = target.complete(id, outcome);
    }
    if target.failed() {
        error = Some(io::Error::other("input cleanup failed"));
    }
    arena.lock().unwrap().stop();
    loop {
        let ready = arena.lock().unwrap().poll_shutdown_ready();
        match ready {
            Ok(true) => break,
            Ok(false) => thread::sleep(Duration::from_millis(5)),
            Err(e) => {
                error = Some(io::Error::other(e));
                break;
            }
        }
    }
    error.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;
    // Review #57: an accept-thread panic cannot bypass joining the pump,
    // including the private shutdown path used by Drop. Use real thread owners.
    #[test]
    fn shutdown_joins_the_pump_after_accept_panics() {
        let endpoint = Endpoint::new(
            jackstay::local::Scope::User,
            &format!("toolkit-join-{}", std::process::id()),
            jackstay::local::Transport::LocalStream,
        )
        .unwrap();
        let listener = Arc::new(Listener::bind(&endpoint).unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let finished = Arc::new(AtomicBool::new(false));
        let done = finished.clone();
        let accept = thread::spawn(|| panic!("accept failure"));
        let worker = thread::spawn(move || {
            while !flag.load(Ordering::Acquire) {
                thread::yield_now()
            }
            thread::sleep(Duration::from_millis(50));
            done.store(true, Ordering::Release);
            Ok(())
        });
        let mut source = Source {
            listener,
            stop,
            accept: Some(accept),
            worker: Some(worker),
        };
        let error = source.shutdown().unwrap_err();
        assert!(error.to_string().contains("accept worker panicked"));
        assert!(finished.load(Ordering::Acquire), "shutdown returned before the pump joined");
        assert!(source.is_finished());
    }
}
