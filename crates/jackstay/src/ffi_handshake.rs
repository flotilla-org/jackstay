//! C handshake v3: offers, selections and admissions as typed structs over
//! Local Endpoint connections. The CBOR messages never cross the ABI.
use std::{
    ffi::{CStr, CString, c_char},
    ptr, slice,
};

use crate::{
    ffi::*,
    ffi_affordances::{FtAffordancesHost, FtAffordancesProducer},
    ffi_input::{self, FtInputClient, FtInputServer, FtInputTarget},
    ffi_local::{FtLocalConnection, restore_connection, take_connection},
    handshake::{self, Admission, Bytes, Direction, Kind, Offer, Offered, Refusal, Selected, Track},
    input::Mode,
    local::PeerIdentity,
    model::ClockDomain,
};

pub const FT_TRACK_FROM_SOURCE: u32 = 0;
pub const FT_TRACK_TO_SOURCE: u32 = 1;
pub const FT_TRACK_BOTH: u32 = 2;

pub const FT_TRACK_ADMITTED: u32 = 0;
pub const FT_TRACK_REFUSED_UNKNOWN: u32 = 1;
pub const FT_TRACK_REFUSED_UNAVAILABLE: u32 = 2;
pub const FT_TRACK_REFUSED_NOT_WRITABLE: u32 = 3;

/// One offered track. Strings are NUL-terminated UTF-8.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FtTrackOffer {
    pub id: u64,
    /// `video`, `input`, `affordances` or a reverse-domain app kind.
    pub kind: *const c_char,
    pub label: *const c_char,
    pub descriptor: *const u8,
    pub descriptor_len: usize,
    pub direction: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FtTrackSelection {
    pub id: u64,
    pub required: u32,
    pub write: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FtTrackAdmission {
    pub id: u64,
    /// FT_TRACK_ADMITTED or one FT_TRACK_REFUSED_* value.
    pub result: u32,
    pub reserved: u32,
}

impl From<Admission> for FtTrackAdmission {
    fn from(admission: Admission) -> Self {
        Self {
            id: admission.id,
            result: match admission.refusal {
                None => FT_TRACK_ADMITTED,
                Some(Refusal::Unknown) => FT_TRACK_REFUSED_UNKNOWN,
                Some(Refusal::Unavailable) => FT_TRACK_REFUSED_UNAVAILABLE,
                Some(Refusal::NotWritable) => FT_TRACK_REFUSED_NOT_WRITABLE,
            },
            reserved: 0,
        }
    }
}

/// A received offer awaiting selection, with C views of its tracks.
pub struct FtHandshakeOffer {
    offered: Offered,
    peer: PeerIdentity,
    source: CString,
    _strings: Vec<CString>,
    tracks: Vec<FtTrackOffer>,
}

fn status(error: handshake::Error) -> FtStatus {
    match error {
        handshake::Error::Input(error) => ffi_input::status(error),
        handshake::Error::Refused { .. } => FT_STATUS_UNSUPPORTED,
        handshake::Error::Io(error) if error.kind() == std::io::ErrorKind::TimedOut => FT_STATUS_TIMEOUT,
        _ => FT_STATUS_ERROR,
    }
}

/// # Safety
/// `text` is null or a live NUL-terminated string.
unsafe fn text(text: *const c_char) -> Option<String> {
    // SAFETY: caller guarantees a live NUL-terminated string when non-null.
    (!text.is_null()).then(|| unsafe { CStr::from_ptr(text) }.to_str().ok().map(str::to_owned))?
}

/// # Safety
/// `track` fields reference live strings and `descriptor_len` readable bytes.
unsafe fn track(track: &FtTrackOffer) -> Option<Track> {
    let descriptor = match track.descriptor_len {
        0 => Vec::new(),
        len if track.descriptor.is_null() || len > handshake::LIMITS.max_byte_string => return None,
        // SAFETY: caller guarantees the readable byte range.
        len => unsafe { slice::from_raw_parts(track.descriptor, len) }.to_vec(),
    };
    Some(Track {
        id: track.id,
        // SAFETY: caller guarantees live strings.
        kind: Kind::from(unsafe { text(track.kind) }?),
        direction: match track.direction {
            FT_TRACK_FROM_SOURCE => Direction::FromSource,
            FT_TRACK_TO_SOURCE => Direction::ToSource,
            FT_TRACK_BOTH => Direction::Both,
            _ => return None,
        },
        // SAFETY: as above.
        label: unsafe { text(track.label) }?,
        descriptor: Bytes(descriptor),
    })
}

/// Offer the host-selected source's tracks and admit the peer's selection.
/// A non-null target authorizes input; NULL refuses any input track.
/// `out_admissions` holds `track_count` entries; `*out_admission_count` gets
/// the number written, in selection order. On success the same connection is
/// returned in `*connection`: serve CPU setup on it when a video track was
/// admitted. After basic checks the connection is consumed on failure,
/// including a required refusal (FT_STATUS_UNSUPPORTED).
///
/// # Safety
/// Pointers are valid and disjoint; `tracks` holds `track_count` live entries;
/// `*connection` is live and exclusively owned; outputs start null; the
/// target is null or live until return and its executor keeps pumping.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_handshake_accept_local(
    connection: *mut *mut FtLocalConnection,
    source_label: *const c_char,
    clock_domain: u32,
    tracks: *const FtTrackOffer,
    track_count: usize,
    authorized_input: *mut FtInputTarget,
    out_admissions: *mut FtTrackAdmission,
    out_admission_count: *mut usize,
    out_input: *mut *mut FtInputServer,
    out_affordances: *mut *mut FtAffordancesProducer,
) -> FtStatus {
    // SAFETY: caller supplies valid, disjoint outputs.
    let (Some(count), Some(input), Some(affordances)) = (unsafe { out_admission_count.as_mut() }, unsafe { out_input.as_mut() }, unsafe {
        out_affordances.as_mut()
    }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let clock = match clock_domain {
        FT_CLOCK_DOMAIN_UNKNOWN => ClockDomain::Unknown,
        FT_CLOCK_DOMAIN_HOST_TIME => ClockDomain::HostTime,
        _ => return FT_STATUS_INVALID_ARGUMENT,
    };
    if !input.is_null() || !affordances.is_null() || (track_count > 0 && (tracks.is_null() || out_admissions.is_null())) {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: caller guarantees `track_count` live entries.
    let entries = if track_count == 0 {
        &[][..]
    } else {
        unsafe { slice::from_raw_parts(tracks, track_count) }
    };
    // SAFETY: entries reference live strings and descriptors.
    let (Some(source), Some(tracks)) = (unsafe { text(source_label) }, entries.iter().map(|t| unsafe { track(t) }).collect()) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let target = unsafe { authorized_input.as_ref() }.map(|target| target.0.clone());
    // SAFETY: the slot is valid; ownership moves exactly once.
    let Some(taken) = (unsafe { take_connection(connection) }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let FtLocalConnection { stream, peer } = *taken;
    match handshake::accept(stream, &Offer { source, clock, tracks }, target) {
        Ok(accepted) => {
            for (index, admission) in accepted.admissions.iter().enumerate() {
                // SAFETY: the host refuses selections longer than its offer.
                unsafe { *out_admissions.add(index) = (*admission).into() };
            }
            *count = accepted.admissions.len();
            *input = accepted
                .input
                .map_or(ptr::null_mut(), |server| Box::into_raw(Box::new(FtInputServer { _server: server })));
            *affordances = accepted
                .affordances
                .map_or(ptr::null_mut(), |p| Box::into_raw(Box::new(FtAffordancesProducer(p))));
            // SAFETY: take_connection nulled this valid slot above.
            unsafe {
                restore_connection(
                    connection,
                    FtLocalConnection {
                        stream: accepted.media,
                        peer,
                    },
                )
            };
            FT_STATUS_OK
        }
        Err(error) => status(error),
    }
}

/// Receive the source's offer on a connection from `ft_local_connect`. After
/// basic checks the connection moves into `*out_offer` (nulled on success and
/// consumed on failure). The five-second bound covers the selection too.
///
/// # Safety
/// `*connection` is live and exclusively owned; `out_offer` is writable,
/// disjoint and starts null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_handshake_connect_local(
    connection: *mut *mut FtLocalConnection,
    out_offer: *mut *mut FtHandshakeOffer,
) -> FtStatus {
    // SAFETY: caller supplies valid output storage.
    let Some(out) = (unsafe { out_offer.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: the slot is valid; ownership moves exactly once.
    let Some(taken) = (unsafe { take_connection(connection) }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let FtLocalConnection { stream, peer } = *taken;
    let offered = match handshake::connect(stream) {
        Ok(offered) => offered,
        Err(error) => return status(error),
    };
    let Ok(source) = CString::new(offered.offer().source.as_str()) else {
        return FT_STATUS_ERROR;
    };
    let mut strings = Vec::new();
    let mut tracks = Vec::new();
    for track in &offered.offer().tracks {
        let (Ok(kind), Ok(label)) = (CString::new(track.kind.as_str()), CString::new(track.label.as_str())) else {
            return FT_STATUS_ERROR;
        };
        tracks.push(FtTrackOffer {
            id: track.id,
            kind: kind.as_ptr(),
            label: label.as_ptr(),
            descriptor: track.descriptor.0.as_ptr(),
            descriptor_len: track.descriptor.0.len(),
            direction: match track.direction {
                Direction::FromSource => FT_TRACK_FROM_SOURCE,
                Direction::ToSource => FT_TRACK_TO_SOURCE,
                Direction::Both => FT_TRACK_BOTH,
            },
            reserved: 0,
        });
        strings.extend([kind, label]);
    }
    *out = Box::into_raw(Box::new(FtHandshakeOffer {
        offered,
        peer,
        source,
        _strings: strings,
        tracks,
    }));
    FT_STATUS_OK
}

/// Borrow the offer's source label and tracks, valid until the offer is
/// selected or destroyed, and copy its FT_CLOCK_DOMAIN_* value.
///
/// # Safety
/// `offer` is live; outputs are writable and disjoint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_handshake_offer_describe(
    offer: *const FtHandshakeOffer,
    out_source_label: *mut *const c_char,
    out_clock_domain: *mut u32,
    out_tracks: *mut *const FtTrackOffer,
    out_track_count: *mut usize,
) -> FtStatus {
    // SAFETY: caller supplies live, disjoint pointers.
    let (Some(offer), Some(source), Some(clock), Some(tracks), Some(count)) = (
        unsafe { offer.as_ref() },
        unsafe { out_source_label.as_mut() },
        unsafe { out_clock_domain.as_mut() },
        unsafe { out_tracks.as_mut() },
        unsafe { out_track_count.as_mut() },
    ) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    *source = offer.source.as_ptr();
    *clock = offer.offered.offer().clock as u32;
    *tracks = offer.tracks.as_ptr();
    *count = offer.tracks.len();
    FT_STATUS_OK
}

/// Select tracks; `input_mode` is one FT_INPUT_MODE_* value, or zero when no
/// input track is selected. `out_admissions` receives `count` entries in
/// selection order. On success the media connection is returned in
/// `*out_connection` for CPU setup, and channels for admitted input and
/// affordances tracks in their outputs. `*out_input_status` is EMPTY without
/// input, OK with an admitted client, or an optional input's clean admission
/// rejection. After basic checks the offer is consumed and nulled on every
/// outcome; a required refusal fails with that status or FT_STATUS_UNSUPPORTED.
///
/// # Safety
/// `*offer` is live and exclusively owned; `selections` holds `count` entries;
/// outputs are writable, disjoint and handle outputs start null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_handshake_select(
    offer: *mut *mut FtHandshakeOffer,
    selections: *const FtTrackSelection,
    count: usize,
    input_mode: u32,
    out_admissions: *mut FtTrackAdmission,
    out_connection: *mut *mut FtLocalConnection,
    out_input: *mut *mut FtInputClient,
    out_input_status: *mut FtStatus,
    out_affordances: *mut *mut FtAffordancesHost,
) -> FtStatus {
    // SAFETY: caller supplies valid, disjoint outputs.
    let (Some(slot), Some(connection), Some(input), Some(input_status), Some(affordances)) = (
        unsafe { offer.as_mut() },
        unsafe { out_connection.as_mut() },
        unsafe { out_input.as_mut() },
        unsafe { out_input_status.as_mut() },
        unsafe { out_affordances.as_mut() },
    ) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if slot.is_null()
        || !connection.is_null()
        || !input.is_null()
        || !affordances.is_null()
        || (count > 0 && (selections.is_null() || out_admissions.is_null()))
    {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: checked non-null; the caller owns this live offer exclusively.
    let tracks = &unsafe { &**slot }.offered.offer().tracks;
    let selection: Vec<Selected> = if count == 0 {
        &[][..]
    } else {
        unsafe { slice::from_raw_parts(selections, count) }
    }
    .iter()
    .map(|s| Selected {
        id: s.id,
        required: s.required != 0,
        write: s.write != 0,
    })
    .collect();
    let input_selected = selection
        .iter()
        .any(|s| tracks.iter().any(|t| t.id == s.id && t.kind == Kind::Input));
    // Without input selected the mode is unused.
    let Some(mode) = ffi_input::mode(input_mode).or((!input_selected && input_mode == 0).then_some(Mode::Cooperative)) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    // SAFETY: this API allocated the box; ownership moves exactly once.
    let FtHandshakeOffer { offered, peer, .. } = *unsafe { Box::from_raw(std::mem::replace(slot, ptr::null_mut())) };
    *input_status = FT_STATUS_EMPTY;
    match offered.select(&selection, mode) {
        Ok(connected) => {
            for (index, admission) in connected.admissions.iter().enumerate() {
                // SAFETY: the reply answers exactly `count` selections.
                unsafe { *out_admissions.add(index) = (*admission).into() };
            }
            *input_status = connected.input_error.map_or(
                if connected.input.is_some() { FT_STATUS_OK } else { FT_STATUS_EMPTY },
                ffi_input::status,
            );
            *input = connected
                .input
                .map_or(ptr::null_mut(), |c| Box::into_raw(Box::new(FtInputClient(c))));
            *affordances = connected
                .affordances
                .map_or(ptr::null_mut(), |h| Box::into_raw(Box::new(FtAffordancesHost(h))));
            *connection = Box::into_raw(Box::new(FtLocalConnection {
                stream: connected.media,
                peer,
            }));
            FT_STATUS_OK
        }
        Err(error) => status(error),
    }
}

/// Abandon an offer without selecting, closing its connection.
///
/// # Safety
/// `offer` is null or points to null or a live, exclusively owned offer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_handshake_offer_destroy(offer: *mut *mut FtHandshakeOffer) {
    // SAFETY: caller guarantees a valid slot; ownership moves exactly once.
    if let Some(slot) = unsafe { offer.as_mut() }
        && !slot.is_null()
    {
        drop(unsafe { Box::from_raw(std::mem::replace(slot, ptr::null_mut())) });
    }
}
