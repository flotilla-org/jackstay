//! An optional CPU publication beside the ingress half's native one.
//!
//! Consumers that speak the generic CPU setup socket (the SDL viewer's
//! `--cpu-socket`, katzensteg's `jackstay-source`) attach here. Each decoded
//! frame is read back from the BGRA staging surface into a CPU arena, one
//! locked copy per frame; on Apple silicon the surface is in unified memory so
//! this is a memcpy, not a bus readback. The socket is bound fresh (never
//! replacing another publication's path), owner-only, and unlinked on
//! shutdown. Setup for each accepted connection runs on its own thread with the
//! transport core's `serve_cpu`.

use std::{
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use jackstay::{
    acquisition::{
        arena::{ArenaConfig, ArenaProducer, FrameDescriptor, PublishOutcome, ReconfigurationStatus},
        socket::serve_cpu,
    },
    model::{ClockDomain, ColorSpace, DamageKind, FrameSyncKind, PayloadKind, PixelFormat},
    native::macos::IoSurface,
};

#[derive(Debug, thiserror::Error)]
pub enum CpuPublicationError {
    #[error("cpu socket {path}: {source}")]
    Bind { path: PathBuf, source: std::io::Error },
    #[error("cpu arena: {0}")]
    Arena(String),
    #[error("surface readback: {0}")]
    Readback(String),
}

fn arena<E: std::fmt::Display>(e: E) -> CpuPublicationError {
    CpuPublicationError::Arena(e.to_string())
}

type Producer = Arc<Mutex<ArenaProducer>>;

pub struct CpuPublication {
    path: PathBuf,
    producer: Producer,
    connections: Arc<Mutex<Vec<UnixStream>>>,
    stop: Arc<AtomicBool>,
    accept_thread: Option<std::thread::JoinHandle<()>>,
    pixels: Vec<u8>,
    /// Payload size the arena currently holds, and one waiting on capacity.
    installed: usize,
    pending: Option<usize>,
    drain_timeout: Duration,
    pub published: AtomicU64,
    pub dropped: AtomicU64,
    pub errors: AtomicU64,
}

impl CpuPublication {
    /// Binds `path`, sized for `width`x`height` BGRA frames, and starts
    /// accepting setup connections. Fails if `path` exists.
    pub fn bind(path: &Path, arena_config: &ArenaConfig, width: u32, height: u32) -> Result<Self, CpuPublicationError> {
        let payload = frame_bytes(width, height);
        let producer = Arc::new(Mutex::new(
            ArenaProducer::new(ArenaConfig {
                payload_capacity: payload,
                ..*arena_config
            })
            .map_err(arena)?,
        ));
        let bind = |source| CpuPublicationError::Bind {
            path: path.to_path_buf(),
            source,
        };
        if path.exists() {
            return Err(bind(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "path exists; another publication may own it",
            )));
        }
        let listener = UnixListener::bind(path).map_err(bind)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(bind)?;
        listener.set_nonblocking(true).map_err(bind)?;
        let stop = Arc::new(AtomicBool::new(false));
        let connections: Arc<Mutex<Vec<UnixStream>>> = Arc::new(Mutex::new(Vec::new()));
        let accept_thread = {
            let stop = stop.clone();
            let producer = producer.clone();
            let connections = connections.clone();
            std::thread::Builder::new()
                .name("jackstay-ingress-cpu-accept".into())
                .spawn(move || accept_loop(listener, producer, connections, stop))
                .map_err(bind)?
        };
        Ok(Self {
            path: path.to_path_buf(),
            producer,
            connections,
            stop,
            accept_thread: Some(accept_thread),
            pixels: Vec::new(),
            installed: payload,
            pending: None,
            drain_timeout: arena_config.drain_timeout,
            published: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads `surface` back and publishes it as frame `sequence` at `timestamp_ns`
    /// (host time). A size change reconfigures the arena first; while that
    /// waits on capacity the frame is dropped.
    pub fn publish(&mut self, surface: &IoSurface, sequence: u64, timestamp_ns: u64) -> Result<PublishOutcome, CpuPublicationError> {
        let (width, height) = (surface.width(), surface.height());
        let bytes = frame_bytes(width, height);
        if bytes != self.installed && self.pending != Some(bytes) {
            let mut producer = self.producer.lock().expect("cpu producer poisoned");
            match producer.reconfigure_cpu(bytes).map_err(arena)? {
                ReconfigurationStatus::Ready { .. } => {
                    self.installed = bytes;
                    self.pending = None;
                }
                ReconfigurationStatus::PausedCapacity { .. } => self.pending = Some(bytes),
            }
        }
        if self.pending.is_some() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Ok(PublishOutcome::Dropped);
        }
        self.pixels.resize(bytes, 0);
        surface
            .read_pixels(&mut self.pixels)
            .map_err(|e| CpuPublicationError::Readback(e.to_string()))?;
        let descriptor = FrameDescriptor {
            sequence,
            timestamp_ns,
            width,
            height,
            stride: width * 4,
            pixel_format: PixelFormat::Bgra8Unorm as u32,
            clock_domain: ClockDomain::HostTime as u32,
            color_space: ColorSpace::Srgb as u32,
            sync_kind: FrameSyncKind::CpuCopyComplete as u32,
            damage_kind: DamageKind::FullFrame as u32,
            payload_kind: PayloadKind::CpuShm as u32,
            ..FrameDescriptor::default()
        };
        let outcome = self
            .producer
            .lock()
            .expect("cpu producer poisoned")
            .publish(descriptor, &self.pixels)
            .map_err(arena)?;
        match outcome {
            PublishOutcome::Published { .. } => self.published.fetch_add(1, Ordering::Relaxed),
            PublishOutcome::Dropped => self.dropped.fetch_add(1, Ordering::Relaxed),
        };
        Ok(outcome)
    }

    /// Periodic upkeep: retire released resources and retry a paused resize.
    pub fn maintain(&mut self) {
        let mut producer = self.producer.lock().expect("cpu producer poisoned");
        if let Err(e) = producer.poll_cleanup() {
            eprintln!("ingress cpu: cleanup: {e}");
        }
        if self.pending.is_some() {
            match producer.advance_reconfiguration() {
                Ok(ReconfigurationStatus::Ready { .. }) => {
                    self.installed = self.pending.take().unwrap_or(self.installed);
                }
                Ok(ReconfigurationStatus::PausedCapacity { .. }) => {}
                Err(e) => eprintln!("ingress cpu: reconfiguration: {e}"),
            }
        }
    }

    /// Stops accepting, drains the arena within its timeout, closes setup
    /// connections and unlinks the socket. Idempotent.
    pub fn shutdown(&mut self) {
        if self.stop.swap(true, Ordering::Relaxed) {
            return;
        }
        if let Some(t) = self.accept_thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.path);
        {
            let mut guard = self.producer.lock().expect("cpu producer poisoned");
            guard.stop();
            let started = Instant::now();
            loop {
                match guard.poll_shutdown_ready() {
                    Ok(true) => break,
                    Ok(false) if started.elapsed() < self.drain_timeout => {
                        drop(guard);
                        std::thread::sleep(Duration::from_millis(20));
                        guard = self.producer.lock().expect("cpu producer poisoned");
                    }
                    Ok(false) => {
                        eprintln!("ingress cpu: producer did not drain within the timeout");
                        break;
                    }
                    Err(e) => {
                        eprintln!("ingress cpu: shutdown needs recovery: {e}");
                        break;
                    }
                }
            }
        }
        for stream in self.connections.lock().expect("connections poisoned").drain(..) {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

impl Drop for CpuPublication {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn frame_bytes(width: u32, height: u32) -> usize {
    width as usize * 4 * height as usize
}

fn accept_loop(listener: UnixListener, producer: Producer, connections: Arc<Mutex<Vec<UnixStream>>>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Ok(handle) = stream.try_clone() {
                    connections.lock().expect("connections poisoned").push(handle);
                }
                let producer = producer.clone();
                let _ = std::thread::Builder::new()
                    .name("jackstay-ingress-cpu-setup".into())
                    .spawn(move || {
                        if let Err(e) = serve_cpu(stream, producer) {
                            eprintln!("ingress cpu: setup connection ended: {e}");
                        }
                    });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                eprintln!("ingress cpu: accept: {e}");
                break;
            }
        }
    }
}
