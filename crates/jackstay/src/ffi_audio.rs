//! C playback tracks (`jackstay_audio.h`): thin typed wrappers over
//! [`crate::audio`]. Offers, selections and notices are in `ffi_handshake`.
use std::{ptr, slice, time::Duration};

use crate::{
    audio::{Descriptor, Doorbell, Format, Layout, Playback, PlaybackReader, PlaybackRing, PlaybackWriter, Sample, Settings},
    ffi::*,
    ffi_acquisition::FtAcquisitionCancellation,
};

pub const FT_AUDIO_BLOCK_DISCONTINUITY: u32 = 1;
pub const FT_AUDIO_BLOCK_RECONFIGURATION: u32 = 2;
pub const FT_AUDIO_BLOCK_GAP: u32 = 4;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FtAudioFormat {
    pub rate: u32,
    pub channels: u16,
    /// `FT_AUDIO_LAYOUT_*`: [`Layout`] as `u8`.
    pub layout: u8,
    /// `FT_AUDIO_SAMPLE_*`: [`Sample`] as `u8`.
    pub sample: u8,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FtAudioDescriptor {
    pub format: FtAudioFormat,
    pub block_frames: u32,
    pub blocks: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FtAudioBlock {
    pub cursor: u64,
    pub sample_position: u64,
    pub host_time_ns: u64,
    pub samples: *const u8,
    pub samples_len: usize,
    pub format: FtAudioFormat,
    pub frames: u32,
    pub generation: u32,
    pub flags: u32,
    pub reserved: u32,
}

impl From<Format> for FtAudioFormat {
    fn from(format: Format) -> Self {
        Self {
            rate: format.rate,
            channels: format.channels,
            layout: format.layout as u8,
            sample: format.sample as u8,
        }
    }
}

impl TryFrom<FtAudioFormat> for Format {
    type Error = ();

    fn try_from(format: FtAudioFormat) -> Result<Self, ()> {
        use Layout::*;
        let layout = [Mono, Stereo, Surround21, Quad, Surround51, Surround71, Discrete];
        let sample = [Sample::U8, Sample::S16, Sample::S32, Sample::F32];
        Ok(Self {
            rate: format.rate,
            channels: format.channels,
            layout: *layout.get(usize::from(format.layout).wrapping_sub(1)).ok_or(())?,
            sample: *sample.get(usize::from(format.sample).wrapping_sub(1)).ok_or(())?,
        })
    }
}

impl From<Descriptor> for FtAudioDescriptor {
    fn from(descriptor: Descriptor) -> Self {
        Self {
            format: descriptor.format.into(),
            block_frames: descriptor.block_frames,
            blocks: descriptor.blocks,
        }
    }
}

impl TryFrom<FtAudioDescriptor> for Descriptor {
    type Error = ();

    fn try_from(descriptor: FtAudioDescriptor) -> Result<Self, ()> {
        Ok(Self {
            format: descriptor.format.try_into()?,
            block_frames: descriptor.block_frames,
            blocks: descriptor.blocks,
        })
    }
}

pub struct FtPlaybackWriter(PlaybackWriter);
pub struct FtPlayback(pub(crate) Playback);
pub struct FtPlaybackRing(pub(crate) PlaybackRing);
pub struct FtPlaybackReader(PlaybackReader);
pub struct FtPlaybackDoorbell(pub(crate) Doorbell);

/// Box `value` into an output slot that must start null.
pub(crate) fn emit<T>(out: &mut *mut T, value: T) {
    *out = Box::into_raw(Box::new(value));
}

/// # Safety
/// Pointers are valid and disjoint; handle outputs start null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_playback_writer_create(
    track_id: u64,
    format: *const FtAudioFormat,
    block_frames: u32,
    depth_ns: u64,
    out_writer: *mut *mut FtPlaybackWriter,
    out_playback: *mut *mut FtPlayback,
) -> FtStatus {
    // SAFETY: caller supplies valid, disjoint pointers.
    let (Some(format), Some(writer), Some(playback)) = (unsafe { format.as_ref() }, unsafe { out_writer.as_mut() }, unsafe {
        out_playback.as_mut()
    }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let Ok(format) = Format::try_from(*format) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !writer.is_null() || !playback.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    let settings = Settings {
        format,
        block_frames,
        depth: Duration::from_nanos(depth_ns),
    };
    match PlaybackWriter::new(track_id, settings) {
        Ok(created) => {
            emit(playback, FtPlayback(created.playback()));
            emit(writer, FtPlaybackWriter(created));
            FT_STATUS_OK
        }
        Err(crate::audio::Error::Invalid(_)) => FT_STATUS_INVALID_ARGUMENT,
        Err(_) => FT_STATUS_ERROR,
    }
}

/// # Safety
/// `writer` is live and not in use elsewhere; `out` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_playback_writer_descriptor(writer: *const FtPlaybackWriter, out: *mut FtAudioDescriptor) -> FtStatus {
    // SAFETY: caller supplies valid pointers.
    let (Some(writer), Some(out)) = (unsafe { writer.as_ref() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    *out = writer.0.descriptor().into();
    FT_STATUS_OK
}

/// Never waits or allocates.
///
/// # Safety
/// `writer` is live and used by one thread at a time; `samples` holds `len`
/// readable bytes; `out_cursor` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_playback_writer_write(
    writer: *mut FtPlaybackWriter,
    samples: *const u8,
    len: usize,
    host_time_ns: u64,
    out_cursor: *mut u64,
) -> FtStatus {
    // SAFETY: caller supplies a live, exclusively used writer.
    let Some(writer) = (unsafe { writer.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if samples.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: caller guarantees `len` readable bytes.
    match writer.0.write(unsafe { slice::from_raw_parts(samples, len) }, host_time_ns) {
        Ok(cursor) => {
            // SAFETY: null or writable.
            if let Some(out) = unsafe { out_cursor.as_mut() } {
                *out = cursor;
            }
            FT_STATUS_OK
        }
        Err(_) => FT_STATUS_INVALID_ARGUMENT,
    }
}

/// # Safety
/// `writer` is live and used by one thread at a time.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_playback_writer_discontinuity(writer: *mut FtPlaybackWriter) -> FtStatus {
    // SAFETY: as above.
    let Some(writer) = (unsafe { writer.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    writer.0.discontinuity();
    FT_STATUS_OK
}

/// # Safety
/// `writer` is live and used by one thread at a time; `format` is readable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_playback_writer_reconfigure(writer: *mut FtPlaybackWriter, format: *const FtAudioFormat) -> FtStatus {
    // SAFETY: as above.
    let (Some(writer), Some(format)) = (unsafe { writer.as_mut() }, unsafe { format.as_ref() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    match Format::try_from(*format).map(|format| writer.0.reconfigure(format)) {
        Ok(Ok(())) => FT_STATUS_OK,
        _ => FT_STATUS_INVALID_ARGUMENT,
    }
}

/// # Safety
/// `ring` is live; `out` is writable and starts null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_playback_reader_create(
    ring: *const FtPlaybackRing,
    latency_ns: u64,
    out: *mut *mut FtPlaybackReader,
) -> FtStatus {
    // SAFETY: caller supplies valid pointers.
    let (Some(ring), Some(out)) = (unsafe { ring.as_ref() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    emit(
        out,
        FtPlaybackReader(PlaybackReader::new(ring.0.clone(), Duration::from_nanos(latency_ns))),
    );
    FT_STATUS_OK
}

/// # Safety
/// `reader` is live and used by one thread at a time; `out` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_playback_reader_read(reader: *mut FtPlaybackReader, out: *mut FtAudioBlock) -> FtStatus {
    // SAFETY: caller supplies valid pointers.
    let (Some(reader), Some(out)) = (unsafe { reader.as_mut() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let Some(block) = reader.0.read() else {
        return FT_STATUS_EMPTY;
    };
    let flag = |set, flag| if set { flag } else { 0 };
    *out = FtAudioBlock {
        cursor: block.cursor,
        sample_position: block.sample_position,
        host_time_ns: block.host_time_ns,
        samples: block.samples.as_ptr(),
        samples_len: block.samples.len(),
        format: block.format.into(),
        frames: block.frames,
        generation: block.generation,
        flags: flag(block.discontinuity, FT_AUDIO_BLOCK_DISCONTINUITY)
            | flag(block.reconfiguration, FT_AUDIO_BLOCK_RECONFIGURATION)
            | flag(block.gap, FT_AUDIO_BLOCK_GAP),
        reserved: 0,
    };
    FT_STATUS_OK
}

/// # Safety
/// `reader` is live; outputs are writable and disjoint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_playback_reader_counters(
    reader: *const FtPlaybackReader,
    out_available: *mut u64,
    out_gaps: *mut u64,
    out_underruns: *mut u64,
) -> FtStatus {
    // SAFETY: caller supplies valid, disjoint pointers.
    let (Some(reader), Some(available), Some(gaps), Some(underruns)) = (
        unsafe { reader.as_ref() },
        unsafe { out_available.as_mut() },
        unsafe { out_gaps.as_mut() },
        unsafe { out_underruns.as_mut() },
    ) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    (*available, *gaps, *underruns) = (reader.0.available(), reader.0.gaps(), reader.0.underruns());
    FT_STATUS_OK
}

/// Take and clear the changed-track hint; zero for null.
///
/// # Safety
/// `doorbell` is null or live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_playback_doorbell_take(doorbell: *const FtPlaybackDoorbell) -> u64 {
    // SAFETY: caller supplies null or a live doorbell.
    unsafe { doorbell.as_ref() }.map_or(0, |doorbell| doorbell.0.take())
}

/// `u64::MAX` waits indefinitely; zero checks once.
///
/// # Safety
/// Pointers are live and disjoint; one waiter per doorbell; cancellation may
/// run concurrently and outlives this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_playback_doorbell_wait(
    doorbell: *mut FtPlaybackDoorbell,
    cancel: *const FtAcquisitionCancellation,
    timeout_ns: u64,
    out_hint: *mut u64,
) -> FtStatus {
    // SAFETY: caller supplies valid, disjoint pointers.
    let (Some(doorbell), Some(cancel), Some(hint)) = (unsafe { doorbell.as_mut() }, unsafe { cancel.as_ref() }, unsafe {
        out_hint.as_mut()
    }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let timeout = (timeout_ns != u64::MAX).then(|| Duration::from_nanos(timeout_ns));
    match doorbell.0.wait(&cancel.0, timeout) {
        Ok(0) if cancel.0.is_cancelled() => FT_STATUS_CANCELLED,
        Ok(0) => FT_STATUS_TIMEOUT,
        Ok(changed) => {
            *hint = changed;
            FT_STATUS_OK
        }
        Err(_) => FT_STATUS_ERROR,
    }
}

macro_rules! destroy {
    ($($name:ident: $kind:ty),* $(,)?) => {$(
        /// Null input/handle is harmless; the handle is nulled.
        ///
        /// # Safety
        /// Sole ownership of a live handle that nothing is still using.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(handle: *mut *mut $kind) {
            // SAFETY: validity and exclusive ownership are caller obligations.
            if let Some(handle) = unsafe { handle.as_mut() }
                && !handle.is_null()
            {
                // SAFETY: this exact box is consumed once, after clearing it.
                drop(unsafe { Box::from_raw(std::mem::replace(handle, ptr::null_mut())) });
            }
        }
    )*};
}
pub(crate) use destroy;

destroy!(
    ft_playback_writer_destroy: FtPlaybackWriter,
    ft_playback_destroy: FtPlayback,
    ft_playback_ring_destroy: FtPlaybackRing,
    ft_playback_reader_destroy: FtPlaybackReader,
    ft_playback_doorbell_destroy: FtPlaybackDoorbell,
);

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors the `_Static_assert`s in `c_abi_header_smoke.c`.
    #[test]
    fn layouts_match_the_c_header() {
        use std::mem::size_of;
        assert_eq!((size_of::<FtAudioFormat>(), size_of::<FtAudioDescriptor>()), (8, 16));
        assert_eq!(size_of::<FtAudioBlock>(), 48 + 2 * size_of::<usize>());
    }
}
