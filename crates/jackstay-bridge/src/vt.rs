//! Safe wrappers over `vt_shim.m`: hardware encode from an `IoSurface`, hardware
//! decode to a YCbCr pixel buffer, transfer into a BGRA `IoSurface`, and the
//! capability probe the halves run at session start.

use std::{
    ffi::{CStr, c_char, c_void},
    ptr::NonNull,
    sync::Mutex,
};

use jackstay::native::macos::IoSurface;
use jackstay_graph::{Chroma, Codec, CodecCapabilities};

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct JsbStreamInfo {
    profile_idc: u32,
    full_range: u32,
    primaries: u32,
    transfer: u32,
    matrix: u32,
    parameter_set_count: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct JsbCaps {
    hevc_444: i32,
    hevc_420: i32,
    h264_444: i32,
    h264_420: i32,
}

type EncoderOutputFn = unsafe extern "C" fn(*mut c_void, *mut c_void, i32, i32, i32, *const u8, usize, i64);
type DecoderOutputFn = unsafe extern "C" fn(*mut c_void, *mut c_void, i32, *mut c_void, i64);

mod ffi {
    use super::*;
    unsafe extern "C" {
        pub fn jsb_string_free(s: *mut c_char);
        pub fn jsb_bytes_free(b: *mut u8);
        pub fn jsb_encoder_create(
            width: u32,
            height: u32,
            codec: i32,
            profile: *const c_char,
            low_latency: i32,
            bitrate_bps: u32,
            fps: u32,
            colour_tags: i32,
            output: EncoderOutputFn,
            refcon: *mut c_void,
            out: *mut *mut c_void,
        ) -> *mut c_char;
        pub fn jsb_encoder_destroy(enc: *mut c_void);
        pub fn jsb_encoder_using_hardware(enc: *mut c_void) -> i32;
        pub fn jsb_encoder_id(enc: *mut c_void) -> *mut c_char;
        pub fn jsb_encoder_encode(
            enc: *mut c_void,
            surface: *mut c_void,
            pts_ns: i64,
            force_keyframe: i32,
            refcon: *mut c_void,
        ) -> *mut c_char;
        pub fn jsb_encoder_flush(enc: *mut c_void) -> *mut c_char;
        pub fn jsb_encoder_stream_info(enc: *mut c_void, out: *mut JsbStreamInfo) -> *mut c_char;
        pub fn jsb_encoder_copy_parameter_set(enc: *mut c_void, index: u32, out: *mut *mut u8, len: *mut usize) -> *mut c_char;
        pub fn jsb_decoder_create(codec: i32, output: DecoderOutputFn, refcon: *mut c_void, out: *mut *mut c_void) -> *mut c_char;
        pub fn jsb_decoder_destroy(dec: *mut c_void);
        pub fn jsb_decoder_configure(
            dec: *mut c_void,
            sets: *const *const u8,
            lens: *const usize,
            count: u32,
            destination: u32,
        ) -> *mut c_char;
        pub fn jsb_decoder_using_hardware(dec: *mut c_void) -> i32;
        pub fn jsb_decoder_pool_shared(dec: *mut c_void) -> i32;
        pub fn jsb_decoder_decode(dec: *mut c_void, annexb: *const u8, len: usize, pts_ns: i64, refcon: *mut c_void) -> *mut c_char;
        pub fn jsb_decoder_flush(dec: *mut c_void) -> *mut c_char;
        pub fn jsb_pixel_buffer_release(pb: *mut c_void);
        pub fn jsb_pixel_buffer_format(pb: *mut c_void) -> u32;
        pub fn jsb_pixel_buffer_width(pb: *mut c_void) -> u32;
        pub fn jsb_pixel_buffer_height(pb: *mut c_void) -> u32;
        pub fn jsb_transfer_create(out: *mut *mut c_void) -> *mut c_char;
        pub fn jsb_transfer_destroy(t: *mut c_void);
        pub fn jsb_transfer_to_surface(t: *mut c_void, pb: *mut c_void, surface: *mut c_void) -> *mut c_char;
        pub fn jsb_probe_capabilities(out: *mut JsbCaps);
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{operation}: {message}")]
pub struct VtError {
    pub operation: &'static str,
    pub message: String,
}

fn check(operation: &'static str, error: *mut c_char) -> Result<(), VtError> {
    if error.is_null() {
        return Ok(());
    }
    // SAFETY: the shim returns a malloc'd NUL-terminated string we own.
    let message = unsafe { CStr::from_ptr(error) }.to_string_lossy().into_owned();
    unsafe { ffi::jsb_string_free(error) };
    Err(VtError { operation, message })
}

fn codec_code(codec: Codec) -> i32 {
    match codec {
        Codec::Hevc => 1,
        Codec::H264 => 2,
    }
}

/// CoreVideo four-character codes for the biplanar output formats.
pub mod pixel_format {
    pub const YCBCR_444_VIDEO: u32 = u32::from_be_bytes(*b"444v");
    pub const YCBCR_444_FULL: u32 = u32::from_be_bytes(*b"444f");
    pub const YCBCR_420_VIDEO: u32 = u32::from_be_bytes(*b"420v");
    pub const YCBCR_420_FULL: u32 = u32::from_be_bytes(*b"420f");
}

/// The decoder output format whose chroma matches the profile and whose range
/// matches the stream. Mismatching the range makes VideoToolbox keep a private
/// pool and copy every frame (measured: 3 ms to 15-22 ms decode at 1080p).
#[must_use]
pub fn matched_output_format(chroma: Chroma, full_range: bool) -> u32 {
    match (chroma, full_range) {
        (Chroma::Full, true) => pixel_format::YCBCR_444_FULL,
        (Chroma::Full, false) => pixel_format::YCBCR_444_VIDEO,
        (Chroma::Subsampled, true) => pixel_format::YCBCR_420_FULL,
        (Chroma::Subsampled, false) => pixel_format::YCBCR_420_VIDEO,
    }
}

#[must_use]
pub fn profile_string(codec: Codec, chroma: Chroma) -> &'static str {
    match (codec, chroma) {
        (Codec::Hevc, Chroma::Full) => "HEVC_Main444_AutoLevel",
        (Codec::Hevc, Chroma::Subsampled) => "HEVC_Main_AutoLevel",
        (Codec::H264, Chroma::Full) => "H264_High444Predictive_AutoLevel",
        (Codec::H264, Chroma::Subsampled) => "H264_High_AutoLevel",
    }
}

/// Runs the encode-and-decode probe for every codec and chroma pair with
/// hardware required on both sides. Takes a few hundred milliseconds.
#[must_use]
pub fn probe_capabilities() -> CodecCapabilities {
    let mut caps = JsbCaps::default();
    // SAFETY: out-pointer to a live struct.
    unsafe { ffi::jsb_probe_capabilities(&mut caps) };
    CodecCapabilities {
        hevc_444_hardware: caps.hevc_444 == 1,
        hevc_420_hardware: caps.hevc_420 == 1,
        h264_444_hardware: caps.h264_444 == 1,
        h264_420_hardware: caps.h264_420 == 1,
    }
}

// ---- encoder --------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub codec: Codec,
    pub chroma: Chroma,
    pub low_latency: bool,
    pub bitrate_bps: u32,
    pub fps: u32,
}

/// One encoded access unit as delivered by VideoToolbox's output thread.
#[derive(Debug)]
pub struct EncodedFrame {
    /// The refcon passed to [`Encoder::encode`], handed back untouched.
    pub refcon: *mut c_void,
    pub status: i32,
    pub keyframe: bool,
    pub dropped: bool,
    /// Annex B access unit without parameter sets. Empty on error or drop.
    pub annexb: Vec<u8>,
    pub pts_ns: i64,
}

// SAFETY: the refcon is an opaque pointer the caller round-trips; the frame owns no other resource.
unsafe impl Send for EncodedFrame {}

type EncoderSink = Box<dyn FnMut(EncodedFrame) + Send>;

struct EncoderState {
    sink: Mutex<EncoderSink>,
}

unsafe extern "C" fn encoder_trampoline(
    refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: i32,
    keyframe: i32,
    dropped: i32,
    annexb: *const u8,
    len: usize,
    pts_ns: i64,
) {
    // SAFETY: refcon is the Box<EncoderState> the encoder holds for its lifetime; the shim
    // never invokes the callback after the session is invalidated in Drop.
    let state = unsafe { &*refcon.cast::<EncoderState>() };
    let bytes = if annexb.is_null() || len == 0 {
        Vec::new()
    } else {
        // SAFETY: the shim guarantees `len` readable bytes for the duration of the call.
        unsafe { std::slice::from_raw_parts(annexb, len) }.to_vec()
    };
    let frame = EncodedFrame {
        refcon: frame_refcon,
        status,
        keyframe: keyframe != 0,
        dropped: dropped != 0,
        annexb: bytes,
        pts_ns,
    };
    if let Ok(mut sink) = state.sink.lock() {
        sink(frame);
    }
}

/// Facts about the stream as produced; valid after the first output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamInfo {
    pub profile_idc: u32,
    pub full_range: bool,
    pub colour: (u8, u8, u8),
    pub parameter_sets: Vec<Vec<u8>>,
}

pub struct Encoder {
    raw: NonNull<c_void>,
    state: *mut EncoderState,
    config: EncoderConfig,
}

// SAFETY: VideoToolbox sessions are safe to drive and query from any thread; the callback
// state marshals through a Mutex and the state box lives as long as the session.
unsafe impl Send for Encoder {}
unsafe impl Sync for Encoder {}

impl std::fmt::Debug for Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Encoder").field("config", &self.config).finish_non_exhaustive()
    }
}

impl Encoder {
    /// Creates a hardware-required session. `sink` runs on VideoToolbox's output
    /// thread for every frame, error and drop; keep it short.
    pub fn new(config: EncoderConfig, sink: EncoderSink) -> Result<Self, VtError> {
        let state = Box::into_raw(Box::new(EncoderState { sink: Mutex::new(sink) }));
        let profile = std::ffi::CString::new(profile_string(config.codec, config.chroma)).expect("static profile");
        let mut raw: *mut c_void = std::ptr::null_mut();
        // SAFETY: all pointers are valid for the call; the state outlives the session.
        let result = check("encoder create", unsafe {
            ffi::jsb_encoder_create(
                config.width,
                config.height,
                codec_code(config.codec),
                profile.as_ptr(),
                i32::from(config.low_latency),
                config.bitrate_bps,
                config.fps,
                1,
                encoder_trampoline,
                state.cast(),
                &mut raw,
            )
        });
        if let Err(e) = result {
            // SAFETY: state was never handed to a live session.
            drop(unsafe { Box::from_raw(state) });
            return Err(e);
        }
        Ok(Self {
            raw: NonNull::new(raw).expect("shim returned NULL encoder without error"),
            state,
            config,
        })
    }

    #[must_use]
    pub fn config(&self) -> EncoderConfig {
        self.config
    }

    /// `Some(true)` when the session reports hardware; `None` for encoders that
    /// never report it (the low-latency ones). Trust the SPS in that case.
    #[must_use]
    pub fn using_hardware(&self) -> Option<bool> {
        match unsafe { ffi::jsb_encoder_using_hardware(self.raw.as_ptr()) } {
            1 => Some(true),
            0 => Some(false),
            _ => None,
        }
    }

    #[must_use]
    pub fn encoder_id(&self) -> String {
        let s = unsafe { ffi::jsb_encoder_id(self.raw.as_ptr()) };
        let out = unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned();
        unsafe { ffi::jsb_string_free(s) };
        out
    }

    /// Submits one surface. `refcon` comes back in the sink's [`EncodedFrame`];
    /// it is how the egress half releases the Jackstay lease once VideoToolbox is
    /// done reading the surface. The pointer is opaque here: neither this crate
    /// nor the shim dereferences it.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn encode(&self, surface: &IoSurface, pts_ns: i64, force_keyframe: bool, refcon: *mut c_void) -> Result<(), VtError> {
        check("encode", unsafe {
            ffi::jsb_encoder_encode(self.raw.as_ptr(), surface.as_raw(), pts_ns, i32::from(force_keyframe), refcon)
        })
    }

    pub fn flush(&self) -> Result<(), VtError> {
        check("flush", unsafe { ffi::jsb_encoder_flush(self.raw.as_ptr()) })
    }

    pub fn stream_info(&self) -> Result<StreamInfo, VtError> {
        let mut info = JsbStreamInfo::default();
        check("stream info", unsafe { ffi::jsb_encoder_stream_info(self.raw.as_ptr(), &mut info) })?;
        let mut sets = Vec::with_capacity(info.parameter_set_count as usize);
        for index in 0..info.parameter_set_count {
            let mut bytes: *mut u8 = std::ptr::null_mut();
            let mut len = 0usize;
            check("parameter set", unsafe {
                ffi::jsb_encoder_copy_parameter_set(self.raw.as_ptr(), index, &mut bytes, &mut len)
            })?;
            // SAFETY: the shim returned `len` bytes we own.
            let set = unsafe { std::slice::from_raw_parts(bytes, len) }.to_vec();
            unsafe { ffi::jsb_bytes_free(bytes) };
            sets.push(set);
        }
        Ok(StreamInfo {
            profile_idc: info.profile_idc,
            full_range: info.full_range != 0,
            colour: (info.primaries as u8, info.transfer as u8, info.matrix as u8),
            parameter_sets: sets,
        })
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // Invalidating the session inside destroy stops further callbacks before the state goes.
        unsafe { ffi::jsb_encoder_destroy(self.raw.as_ptr()) };
        // SAFETY: created by Box::into_raw in `new`; no callback can run after destroy.
        drop(unsafe { Box::from_raw(self.state) });
    }
}

// ---- decoder ---------------------------------------------------------------------

/// A decoded frame in the decoder's own pool. Recycled by VideoToolbox when the
/// last reference goes, so hold it exactly as long as the transfer needs.
pub struct PixelBuffer {
    raw: NonNull<c_void>,
}

// SAFETY: CVPixelBuffer is a thread-safe CF type.
unsafe impl Send for PixelBuffer {}

impl std::fmt::Debug for PixelBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PixelBuffer")
            .field("format", &fourcc(self.format()))
            .field("width", &self.width())
            .field("height", &self.height())
            .finish()
    }
}

impl PixelBuffer {
    #[must_use]
    pub fn format(&self) -> u32 {
        unsafe { ffi::jsb_pixel_buffer_format(self.raw.as_ptr()) }
    }
    #[must_use]
    pub fn width(&self) -> u32 {
        unsafe { ffi::jsb_pixel_buffer_width(self.raw.as_ptr()) }
    }
    #[must_use]
    pub fn height(&self) -> u32 {
        unsafe { ffi::jsb_pixel_buffer_height(self.raw.as_ptr()) }
    }
}

impl Drop for PixelBuffer {
    fn drop(&mut self) {
        unsafe { ffi::jsb_pixel_buffer_release(self.raw.as_ptr()) };
    }
}

#[must_use]
pub fn fourcc(v: u32) -> String {
    String::from_utf8_lossy(&v.to_be_bytes()).into_owned()
}

#[derive(Debug)]
pub struct DecodedFrame {
    pub refcon: *mut c_void,
    pub status: i32,
    pub image: Option<PixelBuffer>,
    pub pts_ns: i64,
}

// SAFETY: as for EncodedFrame.
unsafe impl Send for DecodedFrame {}

type DecoderSink = Box<dyn FnMut(DecodedFrame) + Send>;

struct DecoderState {
    sink: Mutex<DecoderSink>,
}

unsafe extern "C" fn decoder_trampoline(refcon: *mut c_void, frame_refcon: *mut c_void, status: i32, image: *mut c_void, pts_ns: i64) {
    // SAFETY: as for the encoder trampoline.
    let state = unsafe { &*refcon.cast::<DecoderState>() };
    let frame = DecodedFrame {
        refcon: frame_refcon,
        status,
        image: NonNull::new(image).map(|raw| PixelBuffer { raw }),
        pts_ns,
    };
    if let Ok(mut sink) = state.sink.lock() {
        sink(frame);
    }
}

pub struct Decoder {
    raw: NonNull<c_void>,
    state: *mut DecoderState,
    codec: Codec,
}

// SAFETY: as for Encoder.
unsafe impl Send for Decoder {}
unsafe impl Sync for Decoder {}

impl std::fmt::Debug for Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decoder").field("codec", &self.codec).finish_non_exhaustive()
    }
}

impl Decoder {
    pub fn new(codec: Codec, sink: DecoderSink) -> Result<Self, VtError> {
        let state = Box::into_raw(Box::new(DecoderState { sink: Mutex::new(sink) }));
        let mut raw: *mut c_void = std::ptr::null_mut();
        let result = check("decoder create", unsafe {
            ffi::jsb_decoder_create(codec_code(codec), decoder_trampoline, state.cast(), &mut raw)
        });
        if let Err(e) = result {
            drop(unsafe { Box::from_raw(state) });
            return Err(e);
        }
        Ok(Self {
            raw: NonNull::new(raw).expect("shim returned NULL decoder without error"),
            state,
            codec,
        })
    }

    /// Installs parameter sets and opens the session with hardware required,
    /// decoding into `destination` (see [`matched_output_format`]). Reopens only
    /// when the format description or destination changed.
    pub fn configure(&self, parameter_sets: &[Vec<u8>], destination: u32) -> Result<(), VtError> {
        let ptrs: Vec<*const u8> = parameter_sets.iter().map(|s| s.as_ptr()).collect();
        let lens: Vec<usize> = parameter_sets.iter().map(Vec::len).collect();
        check("configure", unsafe {
            ffi::jsb_decoder_configure(
                self.raw.as_ptr(),
                ptrs.as_ptr(),
                lens.as_ptr(),
                u32::try_from(ptrs.len()).expect("parameter set count"),
                destination,
            )
        })
    }

    #[must_use]
    pub fn using_hardware(&self) -> Option<bool> {
        match unsafe { ffi::jsb_decoder_using_hardware(self.raw.as_ptr()) } {
            1 => Some(true),
            0 => Some(false),
            _ => None,
        }
    }

    /// `Some(false)` means VideoToolbox is copying every frame between pools.
    #[must_use]
    pub fn pool_shared(&self) -> Option<bool> {
        match unsafe { ffi::jsb_decoder_pool_shared(self.raw.as_ptr()) } {
            1 => Some(true),
            0 => Some(false),
            _ => None,
        }
    }

    /// Submits one Annex B access unit. `refcon` is opaque and comes back in the
    /// sink's [`DecodedFrame`]; nothing dereferences it on this side of the callback.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn decode(&self, annexb: &[u8], pts_ns: i64, refcon: *mut c_void) -> Result<(), VtError> {
        check("decode", unsafe {
            ffi::jsb_decoder_decode(self.raw.as_ptr(), annexb.as_ptr(), annexb.len(), pts_ns, refcon)
        })
    }

    pub fn flush(&self) -> Result<(), VtError> {
        check("decoder flush", unsafe { ffi::jsb_decoder_flush(self.raw.as_ptr()) })
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { ffi::jsb_decoder_destroy(self.raw.as_ptr()) };
        drop(unsafe { Box::from_raw(self.state) });
    }
}

// ---- transfer -----------------------------------------------------------------------

/// Converts decoded YCbCr pixel buffers into BGRA surfaces. Synchronous.
pub struct Transfer {
    raw: NonNull<c_void>,
}

// SAFETY: a VTPixelTransferSession is safe to use from one thread at a time.
unsafe impl Send for Transfer {}

impl std::fmt::Debug for Transfer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Transfer")
    }
}

impl Transfer {
    pub fn new() -> Result<Self, VtError> {
        let mut raw: *mut c_void = std::ptr::null_mut();
        check("transfer create", unsafe { ffi::jsb_transfer_create(&mut raw) })?;
        Ok(Self {
            raw: NonNull::new(raw).expect("shim returned NULL transfer without error"),
        })
    }

    pub fn to_surface(&self, image: &PixelBuffer, surface: &IoSurface) -> Result<(), VtError> {
        check("transfer", unsafe {
            ffi::jsb_transfer_to_surface(self.raw.as_ptr(), image.raw.as_ptr(), surface.as_raw())
        })
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        unsafe { ffi::jsb_transfer_destroy(self.raw.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, mpsc};

    use jackstay::model::PixelFormat;

    use super::*;

    fn pattern(width: u32, height: u32, seed: u8) -> Vec<u8> {
        let mut px = vec![0u8; (width * height * 4) as usize];
        for y in 0..height {
            for x in 0..width {
                let o = ((y * width + x) * 4) as usize;
                let on = ((x / 8 + y / 8) % 2) == 0;
                px[o] = if on { 255 } else { seed }; // B
                px[o + 1] = if on { seed } else { 255 }; // G
                px[o + 2] = (x % 256) as u8; // R
                px[o + 3] = 255;
            }
        }
        px
    }

    #[test]
    fn probe_reports_something_for_hevc_420() {
        let caps = probe_capabilities();
        // Every Apple Silicon machine encodes and decodes HEVC 4:2:0 in hardware.
        assert!(caps.hevc_420_hardware, "{caps:?}");
    }

    #[test]
    fn hevc_444_round_trips_through_hardware_into_a_bgra_surface() {
        let caps = probe_capabilities();
        if !caps.hevc_444_hardware {
            eprintln!("skipping: no hardware HEVC 4:4:4 on this machine");
            return;
        }
        let (width, height) = (128u32, 96u32);
        let (tx, rx) = mpsc::channel();
        let encoder = Encoder::new(
            EncoderConfig {
                width,
                height,
                codec: Codec::Hevc,
                chroma: Chroma::Full,
                low_latency: false,
                bitrate_bps: 8_000_000,
                fps: 30,
            },
            Box::new(move |f| {
                let _ = tx.send(f);
            }),
        )
        .unwrap();
        assert_eq!(encoder.using_hardware(), Some(true));
        let sources: Vec<IoSurface> = (0..6u8)
            .map(|i| {
                let s = IoSurface::allocate(width, height, PixelFormat::Bgra8Unorm).unwrap();
                s.write_pixels(&pattern(width, height, i * 40)).unwrap();
                s
            })
            .collect();
        for (i, s) in sources.iter().enumerate() {
            encoder
                .encode(s, i as i64 * 33_000_000, i == 0, std::ptr::without_provenance_mut(i + 1))
                .unwrap();
        }
        encoder.flush().unwrap();
        let mut frames: Vec<EncodedFrame> = Vec::new();
        while let Ok(f) = rx.recv_timeout(std::time::Duration::from_secs(5)) {
            frames.push(f);
            if frames.len() == sources.len() {
                break;
            }
        }
        assert_eq!(frames.len(), sources.len());
        assert!(frames[0].keyframe && !frames[0].annexb.is_empty());
        assert_eq!(frames[0].refcon, std::ptr::without_provenance_mut(1));
        let info = encoder.stream_info().unwrap();
        assert_eq!(info.profile_idc, 4, "RExt");
        assert_eq!(info.parameter_sets.len(), 3, "VPS, SPS, PPS");

        let decoded = Arc::new(Mutex::new(Vec::<DecodedFrame>::new()));
        let sink = decoded.clone();
        let decoder = Decoder::new(
            Codec::Hevc,
            Box::new(move |f| {
                sink.lock().unwrap().push(f);
            }),
        )
        .unwrap();
        decoder
            .configure(&info.parameter_sets, matched_output_format(Chroma::Full, info.full_range))
            .unwrap();
        assert_eq!(decoder.using_hardware(), Some(true));
        assert_eq!(decoder.pool_shared(), Some(true), "range-matched output must share the pool");
        for f in &frames {
            decoder.decode(&f.annexb, f.pts_ns, f.refcon).unwrap();
        }
        decoder.flush().unwrap();
        let mut out = decoded.lock().unwrap();
        assert_eq!(out.len(), frames.len());
        let last = out.pop().unwrap();
        let image = last.image.expect("decoded image");
        assert_eq!(fourcc(image.format()), if info.full_range { "444f" } else { "444v" });
        let transfer = Transfer::new().unwrap();
        let target = IoSurface::allocate(width, height, PixelFormat::Bgra8Unorm).unwrap();
        transfer.to_surface(&image, &target).unwrap();
        let mut got = vec![0u8; (width * height * 4) as usize];
        target.read_pixels(&mut got).unwrap();
        let want = pattern(width, height, 5 * 40);
        let mut worst = 0i32;
        let mut sum = 0u64;
        for (g, w) in got.chunks(4).zip(want.chunks(4)) {
            for c in 0..3 {
                let d = (i32::from(g[c]) - i32::from(w[c])).abs();
                worst = worst.max(d);
                sum += d as u64;
            }
        }
        let mean = sum as f64 / (width * height * 3) as f64;
        assert!(mean < 3.0, "mean abs error {mean} (worst {worst})");
    }
}
