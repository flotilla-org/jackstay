//! Playback audio tracks. Each track is one single-writer seqlock broadcast
//! ring of fixed-size sample blocks, following `jackstay_ring.h`: the writer
//! zeroes a block's sequence word, writes it, then publishes the sequence and
//! the ring cursor; a reader copies a block out and keeps it only if the
//! sequence named its cursor before and after the copy. Reader cursors are
//! private, so a slow reader is lapped without affecting anyone else, and the
//! writer never waits. Consumers map rings read-only.
//!
//! Each consumer incarnation that selects playback tracks gets one doorbell:
//! a shared changed-track hint (bit = selection position mod 64) and the
//! coalescing wake from `acquisition::arena`. The hint is optional; readers may
//! always check their cursors with [`PlaybackReader::available`].

#[cfg(unix)]
use std::os::fd::OwnedFd;
#[cfg(windows)]
use std::os::windows::io::OwnedHandle as OwnedFd;
use std::{
    io,
    mem::size_of,
    sync::{
        Arc, Mutex, PoisonError, Weak,
        atomic::{AtomicU64, Ordering::*, fence},
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::{
    CaptureTransferError,
    acquisition::arena::{Cancellation, Receiver, Wake, channel},
    handshake::{Bytes, Direction, Kind, LIMITS, Track},
    shm::SharedMemorySegment,
};

/// The shortest ring a producer may configure.
pub const MIN_DEPTH: Duration = Duration::from_millis(250);

const MAGIC: u64 = u64::from_le_bytes(*b"JSAUDIO1");
const VERSION: u64 = 1;
/// Line 0 holds the immutable [`RingHeader`]; line 1 the write cursor.
const HEADER_LEN: usize = 256;
const CURSOR: usize = 128;
const BLOCK_ALIGN: usize = 64;
const BELL_LEN: usize = 64;
const DISCONTINUITY: u32 = 1;
const RECONFIGURATION: u32 = 2;

/// Channel layout. Positioned layouts use SDL/WAVE channel order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum Layout {
    Mono = 1,
    Stereo,
    #[serde(rename = "2.1")]
    Surround21,
    Quad,
    #[serde(rename = "5.1")]
    Surround51,
    #[serde(rename = "7.1")]
    Surround71,
    /// Any channel count, without positions.
    Discrete,
}

/// Little-endian interleaved sample format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum Sample {
    U8 = 1,
    S16,
    S32,
    F32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Format {
    /// Hz.
    pub rate: u32,
    pub channels: u16,
    pub layout: Layout,
    pub sample: Sample,
}

impl Format {
    #[must_use]
    pub fn frame_bytes(&self) -> usize {
        usize::from(self.channels)
            * match self.sample {
                Sample::U8 => 1,
                Sample::S16 => 2,
                Sample::S32 | Sample::F32 => 4,
            }
    }

    fn valid(&self) -> bool {
        let positioned = [1, 2, 3, 4, 6, 8, self.channels][self.layout as usize - 1];
        self.rate > 0 && self.channels > 0 && self.channels == positioned
    }

    fn from_block(block: &BlockHeader) -> Option<Self> {
        use Layout::*;
        let layout = [Mono, Stereo, Surround21, Quad, Surround51, Surround71, Discrete].get(usize::from(block.layout).checked_sub(1)?)?;
        let sample = [Sample::U8, Sample::S16, Sample::S32, Sample::F32].get(usize::from(block.sample).checked_sub(1)?)?;
        let format = Self {
            rate: block.rate,
            channels: block.channels,
            layout: *layout,
            sample: *sample,
        };
        format.valid().then_some(format)
    }
}

/// A playback track's offer descriptor: its current format and ring geometry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Descriptor {
    pub format: Format,
    /// Most frames one block holds in this format.
    pub block_frames: u32,
    /// Ring depth in blocks.
    pub blocks: u32,
}

impl Descriptor {
    pub fn decode(bytes: &Bytes) -> Result<Self, flotilla_cbor::Error> {
        flotilla_cbor::from_slice(&bytes.0, &LIMITS)
    }
}

/// A producer's choice of format, block size (its period) and ring depth.
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub format: Format,
    pub block_frames: u32,
    /// At least [`MIN_DEPTH`]; rounded up to whole blocks.
    pub depth: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid playback track: {0}")]
    Invalid(&'static str),
    #[error(transparent)]
    Storage(#[from] CaptureTransferError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Ring line 0, written once before the ring is shared.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct RingHeader {
    magic: u64,
    version: u64,
    blocks: u64,
    block_len: u64,
    payload_len: u64,
    map_len: u64,
}

/// One block: this header, then up to `payload_len` bytes of samples.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BlockHeader {
    /// Seqlock word: 0 while being written, else this block's cursor.
    sequence: u64,
    sample_position: u64,
    host_time_ns: u64,
    frames: u32,
    flags: u32,
    generation: u32,
    rate: u32,
    channels: u16,
    layout: u8,
    sample: u8,
    reserved: [u8; 20],
}

const _: () = assert!(size_of::<RingHeader>() <= CURSOR && size_of::<BlockHeader>() == BLOCK_ALIGN);

#[derive(Debug)]
struct Ring {
    segment: SharedMemorySegment,
    blocks: u64,
    block_len: usize,
    payload_len: usize,
}

impl Ring {
    fn create(blocks: u64, payload_len: usize) -> Result<(Self, OwnedFd), Error> {
        let block_len = (size_of::<BlockHeader>() + payload_len).next_multiple_of(BLOCK_ALIGN);
        let map_len = usize::try_from(blocks)
            .ok()
            .and_then(|blocks| blocks.checked_mul(block_len)?.checked_add(HEADER_LEN))
            .ok_or(Error::Invalid("ring too large"))?;
        let (segment, read_only) = SharedMemorySegment::with_read_only_fd(map_len)?;
        let header = RingHeader {
            magic: MAGIC,
            version: VERSION,
            blocks,
            block_len: block_len as u64,
            payload_len: payload_len as u64,
            map_len: map_len as u64,
        };
        // SAFETY: the new, zeroed mapping is exclusively owned until shared.
        unsafe { segment.as_ptr().cast_mut().cast::<RingHeader>().write(header) };
        let ring = Self {
            segment,
            blocks,
            block_len,
            payload_len,
        };
        Ok((ring, read_only))
    }

    /// Map a received ring read-only, validating its geometry once.
    fn map(fd: OwnedFd) -> Result<Self, Error> {
        let probe = SharedMemorySegment::map_read_only(fd.try_clone()?, HEADER_LEN)?;
        // SAFETY: line 0 is plain integers, immutable once shared.
        let header = unsafe { probe.as_ptr().cast::<RingHeader>().read() };
        drop(probe);
        let (Ok(block_len), Ok(payload_len), Ok(map_len)) = (
            usize::try_from(header.block_len),
            usize::try_from(header.payload_len),
            usize::try_from(header.map_len),
        ) else {
            return Err(Error::Invalid("ring geometry"));
        };
        let expected = usize::try_from(header.blocks)
            .ok()
            .and_then(|blocks| blocks.checked_mul(block_len)?.checked_add(HEADER_LEN));
        if header.magic != MAGIC
            || header.version != VERSION
            || header.blocks < 2
            || !block_len.is_multiple_of(BLOCK_ALIGN)
            || block_len < size_of::<BlockHeader>().saturating_add(payload_len)
            || expected != Some(map_len)
        {
            return Err(Error::Invalid("ring geometry"));
        }
        let segment = SharedMemorySegment::map_read_only(fd, map_len)?;
        // SAFETY: as above, on the full mapping.
        if unsafe { segment.as_ptr().cast::<RingHeader>().read() } != header {
            return Err(Error::Invalid("ring geometry"));
        }
        Ok(Self {
            segment,
            blocks: header.blocks,
            block_len,
            payload_len,
        })
    }

    fn word(&self, offset: usize) -> &AtomicU64 {
        assert!(offset.is_multiple_of(8) && offset + 8 <= self.segment.len());
        // SAFETY: an aligned, in-bounds word of the live mapping that is only
        // ever accessed atomically.
        unsafe { &*self.segment.as_ptr().add(offset).cast::<AtomicU64>() }
    }

    fn latest(&self) -> u64 {
        self.word(CURSOR).load(Acquire)
    }

    fn block(&self, cursor: u64) -> usize {
        HEADER_LEN + ((cursor - 1) % self.blocks) as usize * self.block_len
    }

    fn publish(&self, cursor: u64, header: &BlockHeader, samples: &[u8]) {
        let offset = self.block(cursor);
        let sequence = self.word(offset);
        sequence.store(0, Relaxed);
        // Keep the invalidating zero ahead of the plain stores.
        fence(Release);
        // SAFETY: the writable mapping holds this whole block; samples fit the
        // payload. Field 0 (the sequence word) is not copied as plain bytes.
        unsafe {
            let block = self.segment.as_ptr().cast_mut().add(offset);
            std::ptr::copy_nonoverlapping(
                std::ptr::from_ref(header).cast::<u8>().add(8),
                block.add(8),
                size_of::<BlockHeader>() - 8,
            );
            std::ptr::copy_nonoverlapping(samples.as_ptr(), block.add(size_of::<BlockHeader>()), samples.len());
        }
        sequence.store(cursor, Release);
        self.word(CURSOR).store(cursor, Release);
    }

    /// Copy one block's header and payload; None if it was not intact.
    fn copy(&self, cursor: u64, payload: &mut [u8]) -> Option<BlockHeader> {
        let offset = self.block(cursor);
        let sequence = self.word(offset);
        let first = sequence.load(Acquire);
        let mut header = BlockHeader::default();
        // SAFETY: in-bounds reads of the mapped block into local storage of
        // the same lengths, skipping the atomic sequence word.
        unsafe {
            let block = self.segment.as_ptr().add(offset);
            std::ptr::copy_nonoverlapping(
                block.add(8),
                std::ptr::from_mut(&mut header).cast::<u8>().add(8),
                size_of::<BlockHeader>() - 8,
            );
            std::ptr::copy_nonoverlapping(block.add(size_of::<BlockHeader>()), payload.as_mut_ptr(), self.payload_len);
        }
        // Resolve the copies before the validating re-read.
        fence(Acquire);
        (first == cursor && sequence.load(Relaxed) == cursor).then_some(header)
    }
}

#[derive(Debug)]
struct Shared {
    id: u64,
    ring: Ring,
    read_only: OwnedFd,
    bells: Mutex<Vec<(Weak<BellInner>, u64)>>,
}

/// The producer side of one playback track. Writing never blocks or
/// allocates, and continues with no readers.
#[derive(Debug)]
pub struct PlaybackWriter {
    shared: Arc<Shared>,
    format: Format,
    generation: u32,
    position: u64,
    cursor: u64,
    flags: u32,
}

impl PlaybackWriter {
    pub fn new(id: u64, settings: Settings) -> Result<Self, Error> {
        let Settings {
            format,
            block_frames,
            depth,
        } = settings;
        if !format.valid() || block_frames == 0 {
            return Err(Error::Invalid("format"));
        }
        if depth < MIN_DEPTH {
            return Err(Error::Invalid("ring depth below 250 ms"));
        }
        let blocks = (depth.as_nanos() * u128::from(format.rate)).div_ceil(u128::from(block_frames) * 1_000_000_000);
        let blocks = u64::try_from(blocks).map_err(|_| Error::Invalid("ring too large"))?.max(2);
        let payload_len = (block_frames as usize)
            .checked_mul(format.frame_bytes())
            .ok_or(Error::Invalid("block too large"))?;
        let (ring, read_only) = Ring::create(blocks, payload_len)?;
        Ok(Self {
            shared: Arc::new(Shared {
                id,
                ring,
                read_only,
                bells: Mutex::default(),
            }),
            format,
            generation: 1,
            position: 0,
            cursor: 0,
            flags: 0,
        })
    }

    #[must_use]
    pub fn descriptor(&self) -> Descriptor {
        let ring = &self.shared.ring;
        Descriptor {
            format: self.format,
            block_frames: (ring.payload_len / self.format.frame_bytes()) as u32,
            blocks: ring.blocks as u32,
        }
    }

    /// The offered track, carrying the current descriptor.
    pub fn track(&self, label: impl Into<String>) -> Track {
        Track {
            id: self.shared.id,
            kind: Kind::Playback,
            direction: Direction::FromSource,
            label: label.into(),
            descriptor: Bytes(flotilla_cbor::to_vec(&self.descriptor(), &LIMITS).expect("a descriptor encodes")),
        }
    }

    /// The handle a source host passes to `handshake::accept`.
    #[must_use]
    pub fn playback(&self) -> Playback {
        Playback(self.shared.clone())
    }

    /// Flag the next block as a discontinuity, for example after a pause.
    pub fn discontinuity(&mut self) {
        self.flags |= DISCONTINUITY;
    }

    /// Switch format at the next block, which carries the reconfiguration
    /// marker and a new generation. Block size in bytes stays fixed.
    pub fn reconfigure(&mut self, format: Format) -> Result<(), Error> {
        if !format.valid() || format.frame_bytes() > self.shared.ring.payload_len {
            return Err(Error::Invalid("format"));
        }
        self.format = format;
        self.generation += 1;
        self.flags |= RECONFIGURATION;
        Ok(())
    }

    /// Publish one block of whole interleaved frames, stamped with the host
    /// time of its first sample, and ring the doorbells. Returns its cursor.
    pub fn write(&mut self, samples: &[u8], host_time_ns: u64) -> Result<u64, Error> {
        let frame = self.format.frame_bytes();
        if samples.is_empty() || !samples.len().is_multiple_of(frame) || samples.len() > self.shared.ring.payload_len {
            return Err(Error::Invalid("block length"));
        }
        let frames = (samples.len() / frame) as u32;
        self.cursor += 1;
        let header = BlockHeader {
            sample_position: self.position,
            host_time_ns,
            frames,
            flags: std::mem::take(&mut self.flags),
            generation: self.generation,
            rate: self.format.rate,
            channels: self.format.channels,
            layout: self.format.layout as u8,
            sample: self.format.sample as u8,
            ..BlockHeader::default()
        };
        self.shared.ring.publish(self.cursor, &header, samples);
        self.position += u64::from(frames);
        // A contended registration skips this block's hint; never wait here.
        if let Ok(mut bells) = self.shared.bells.try_lock() {
            bells.retain(|(bell, bit)| bell.upgrade().is_some_and(|bell| bell.ring(*bit)));
        }
        Ok(self.cursor)
    }
}

/// A source host's handle to one playback track's ring.
#[derive(Clone, Debug)]
pub struct Playback(Arc<Shared>);

impl Playback {
    #[must_use]
    pub fn id(&self) -> u64 {
        self.0.id
    }

    pub(crate) fn read_only_fd(&self) -> io::Result<OwnedFd> {
        self.0.read_only.try_clone()
    }

    pub(crate) fn register(&self, bell: &Bell, position: usize) {
        let mut bells = self.0.bells.lock().unwrap_or_else(PoisonError::into_inner);
        bells.push((Arc::downgrade(&bell.0), 1 << (position % 64)));
    }
}

#[derive(Debug)]
struct BellInner {
    page: SharedMemorySegment,
    wake: Arc<Wake>,
}

impl BellInner {
    /// Set `bit`, waking only for the first bit since the consumer last took
    /// the hint. False once the consumer has gone.
    fn ring(&self, bit: u64) -> bool {
        hint(&self.page).fetch_or(bit, SeqCst) != 0
            || !matches!(self.wake.signal(), Err(error) if matches!(error.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset))
    }
}

/// The host's end of one incarnation's playback doorbell. Retain it for the
/// incarnation's lifetime; dropping it stops the doorbell, not the rings.
#[derive(Debug)]
pub struct Bell(Arc<BellInner>);

impl Bell {
    /// The bell and the consumer's objects: hint page, then wake receiver.
    pub(crate) fn new() -> Result<(Self, Vec<OwnedFd>), Error> {
        let page = SharedMemorySegment::new(BELL_LEN)?;
        let (wake, receiver) = channel()?;
        let objects = vec![page.try_clone_fd()?, receiver.into_fd()];
        Ok((Self(Arc::new(BellInner { page, wake })), objects))
    }
}

fn hint(page: &SharedMemorySegment) -> &AtomicU64 {
    // SAFETY: word 0 of a live page of at least BELL_LEN bytes, only ever
    // accessed atomically.
    unsafe { &*page.as_ptr().cast::<AtomicU64>() }
}

/// A consumer incarnation's doorbell for its selected playback tracks.
#[derive(Debug)]
pub struct Doorbell {
    page: SharedMemorySegment,
    receiver: Receiver,
}

impl Doorbell {
    pub(crate) fn map(page: OwnedFd, receiver: OwnedFd) -> Result<Self, Error> {
        Ok(Self {
            page: SharedMemorySegment::map_read_write(page, BELL_LEN)?,
            receiver: Receiver::from_fd(receiver)?,
        })
    }

    /// Take and clear the changed-track hint: bit `i` means the track at
    /// selection position `i` (mod 64) published since the last take.
    pub fn take(&self) -> u64 {
        hint(&self.page).swap(0, SeqCst)
    }

    /// Sleep until the hint is non-zero, then take it. Returns 0 on
    /// cancellation or timeout.
    pub fn wait(&mut self, cancel: &Cancellation, timeout: Option<Duration>) -> io::Result<u64> {
        let deadline = timeout.and_then(|timeout| Instant::now().checked_add(timeout));
        loop {
            // Drain before taking: a bit set after the take signals again.
            self.receiver.drain()?;
            let changed = self.take();
            if changed != 0 || cancel.is_cancelled() || deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(changed);
            }
            self.receiver.sleep_or_cancel(cancel.receiver(), deadline)?;
        }
    }
}

/// A consumer's read-only mapping of one selected playback track.
#[derive(Clone, Debug)]
pub struct PlaybackRing {
    id: u64,
    descriptor: Descriptor,
    ring: Arc<Ring>,
}

impl PlaybackRing {
    pub(crate) fn map(id: u64, descriptor: Descriptor, fd: OwnedFd) -> Result<Self, Error> {
        let ring = Ring::map(fd)?;
        if ring.blocks != u64::from(descriptor.blocks) || descriptor.block_frames == 0 || !descriptor.format.valid() {
            return Err(Error::Invalid("ring disagrees with its descriptor"));
        }
        Ok(Self {
            id,
            descriptor,
            ring: Arc::new(ring),
        })
    }

    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The descriptor offered at selection.
    #[must_use]
    pub fn descriptor(&self) -> Descriptor {
        self.descriptor
    }

    /// The mapping's read-only descriptor (a section handle on Windows).
    pub fn try_clone_fd(&self) -> crate::Result<OwnedFd> {
        self.ring.segment.try_clone_fd()
    }
}

/// One block copied out of the ring.
#[derive(Debug)]
pub struct Block<'a> {
    pub cursor: u64,
    pub sample_position: u64,
    /// Host time of the first sample.
    pub host_time_ns: u64,
    pub frames: u32,
    pub format: Format,
    pub generation: u32,
    pub discontinuity: bool,
    /// The format changed at this block's first sample.
    pub reconfiguration: bool,
    /// This reader was lapped and resynchronized just before this block.
    pub gap: bool,
    pub samples: &'a [u8],
}

/// A private cursor over one ring. Nothing about it is shared.
#[derive(Debug)]
pub struct PlaybackReader {
    ring: PlaybackRing,
    latency: u64,
    next: u64,
    gap: bool,
    gaps: u64,
    underruns: u64,
    payload: Vec<u8>,
}

impl PlaybackReader {
    /// `latency` is how far behind the writer this reader plays, rounded up
    /// to whole blocks and capped below the ring depth. The reader starts at
    /// (write cursor minus that target) once the ring has a block.
    #[must_use]
    pub fn new(ring: PlaybackRing, latency: Duration) -> Self {
        let Descriptor {
            format,
            block_frames,
            blocks,
        } = ring.descriptor;
        let latency = (latency.as_nanos() * u128::from(format.rate)).div_ceil(u128::from(block_frames) * 1_000_000_000);
        Self {
            latency: latency.min(u128::from(blocks - 1)) as u64,
            payload: vec![0; ring.ring.payload_len],
            ring,
            next: 0,
            gap: false,
            gaps: 0,
            underruns: 0,
        }
    }

    fn start(&self, latest: u64) -> u64 {
        latest.saturating_add(1).saturating_sub(self.latency).max(1)
    }

    /// Blocks ready to read: the cursor check that needs no doorbell.
    #[must_use]
    pub fn available(&self) -> u64 {
        let latest = self.ring.ring.latest();
        latest
            .saturating_add(1)
            .saturating_sub(if self.next == 0 { self.start(latest) } else { self.next })
    }

    /// Delivery gaps: times this reader was lapped and resynchronized.
    #[must_use]
    pub fn gaps(&self) -> u64 {
        self.gaps
    }

    /// Reads that found no block ready.
    #[must_use]
    pub fn underruns(&self) -> u64 {
        self.underruns
    }

    /// The next block in order, or None in underrun. Call it when the output
    /// device needs data.
    pub fn read(&mut self) -> Option<Block<'_>> {
        let ring = &self.ring.ring;
        // A conforming writer cannot lap a resynchronized reader during one
        // copy, so a second failure means a misbehaving one: report underrun.
        for _ in 0..2 {
            let latest = ring.latest();
            if self.next == 0 && latest > 0 {
                self.next = self.start(latest);
            }
            if self.next == 0 || self.next > latest {
                break;
            }
            let block = (latest - self.next < ring.blocks)
                .then(|| ring.copy(self.next, &mut self.payload))
                .flatten()
                .and_then(|block| Some((block, Format::from_block(&block)?)))
                .filter(|(block, format)| block.frames > 0 && block.frames as usize * format.frame_bytes() <= ring.payload_len);
            let Some((block, format)) = block else {
                self.gaps += 1;
                self.gap = true;
                self.next = self.start(ring.latest());
                continue;
            };
            self.next += 1;
            return Some(Block {
                cursor: self.next - 1,
                sample_position: block.sample_position,
                host_time_ns: block.host_time_ns,
                frames: block.frames,
                format,
                generation: block.generation,
                discontinuity: block.flags & DISCONTINUITY != 0,
                reconfiguration: block.flags & RECONFIGURATION != 0,
                gap: std::mem::take(&mut self.gap),
                samples: &self.payload[..block.frames as usize * format.frame_bytes()],
            });
        }
        self.underruns += 1;
        None
    }
}
