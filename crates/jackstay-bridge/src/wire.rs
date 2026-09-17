//! Carrier-agnostic framing for the bridge link.
//!
//! Every message is a 32-byte little-endian header followed by a body. The
//! header is fixed so the same bytes can ride a length-delimited byte stream
//! (one message per read), one QUIC stream per frame, or chunked datagrams
//! (`chunk_index`/`chunk_count`) without renegotiation.
//!
//! Frame bodies are binary: the 144-byte Jackstay [`FrameDescriptor`] verbatim,
//! then an op list over a persistent surface. Version 1 emits exactly one
//! full-frame [`Op::Video`]; copy, pixel-tile and region ops are reserved so a
//! two-tier scheme can be added later without a wire break. Every republished
//! Jackstay frame is a complete image; the ingress half owns the canvas that
//! keeps that true once partial ops exist.
//!
//! Control bodies are JSON: they are small, rare, and the setup channels in the
//! transport core already speak `serde_json`.

use std::io::{self, Read, Write};

use jackstay::acquisition::arena::FrameDescriptor;
use jackstay_graph::{Chroma, ChromaPolicy, Codec, CodecCapabilities, Target};
use serde::{Deserialize, Serialize};

pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 32;
pub const DESCRIPTOR_LEN: usize = 144;
/// Largest body the readers accept. A 4K 4:4:4 keyframe at a high bitrate is
/// well under this; anything larger is a framing error, not a frame.
pub const MAX_BODY: usize = 64 * 1024 * 1024;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Hello = 0,
    Frame = 1,
    CodecConfig = 2,
    Target = 3,
    Release = 4,
    KeyframeRequest = 5,
    ClockPing = 6,
    ClockPong = 7,
    /// A relayed jackstay input transport stream, on the control connection.
    /// `stream_id` names the relayed connection; `flags::INPUT_OPEN` opens it
    /// and `flags::INPUT_CLOSE` closes it after this body; the body is the
    /// transport's bytes untouched (see `input_relay`).
    Input = 8,
}

impl Kind {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Hello,
            1 => Self::Frame,
            2 => Self::CodecConfig,
            3 => Self::Target,
            4 => Self::Release,
            5 => Self::KeyframeRequest,
            6 => Self::ClockPing,
            7 => Self::ClockPong,
            8 => Self::Input,
            _ => return None,
        })
    }
}

pub mod flags {
    pub const KEYFRAME: u16 = 1;
    /// The sender may drop this message under pressure without telling anyone.
    pub const DISCARDABLE: u16 = 2;
    /// A `CodecConfig` preceded this frame because the configuration changed.
    pub const CONFIG_CHANGED: u16 = 4;
    pub const LAST_CHUNK: u16 = 8;
    /// An `Input` message opening a relayed input stream (`stream_id`).
    pub const INPUT_OPEN: u16 = 16;
    /// An `Input` message closing a relayed input stream after this body.
    pub const INPUT_CLOSE: u16 = 32;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub flags: u16,
    pub stream_id: u32,
    /// Frame sequence for frames; a monotonic message counter otherwise. At a
    /// keyframe this is also the group sequence a Media over QUIC carrier uses.
    pub seq: u64,
    /// Producer clock domain, untouched by the egress half.
    pub timestamp_ns: u64,
    pub chunk_index: u16,
    pub chunk_count: u16,
    pub body_len: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub header: Header,
    pub body: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("unsupported wire version {0}")]
    Version(u8),
    #[error("unknown message kind {0}")]
    Kind(u8),
    #[error("body of {0} bytes exceeds the limit")]
    TooLarge(u32),
    #[error("truncated {0}")]
    Truncated(&'static str),
    #[error("unknown op kind {0}")]
    OpKind(u8),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

impl Header {
    #[must_use]
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0] = VERSION;
        out[1] = self.kind as u8;
        out[2..4].copy_from_slice(&self.flags.to_le_bytes());
        out[4..8].copy_from_slice(&self.stream_id.to_le_bytes());
        out[8..16].copy_from_slice(&self.seq.to_le_bytes());
        out[16..24].copy_from_slice(&self.timestamp_ns.to_le_bytes());
        out[24..26].copy_from_slice(&self.chunk_index.to_le_bytes());
        out[26..28].copy_from_slice(&self.chunk_count.to_le_bytes());
        out[28..32].copy_from_slice(&self.body_len.to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8; HEADER_LEN]) -> Result<Self, WireError> {
        if bytes[0] != VERSION {
            return Err(WireError::Version(bytes[0]));
        }
        let kind = Kind::from_u8(bytes[1]).ok_or(WireError::Kind(bytes[1]))?;
        let u16_at = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes(bytes[i..i + 4].try_into().expect("4 bytes"));
        let u64_at = |i: usize| u64::from_le_bytes(bytes[i..i + 8].try_into().expect("8 bytes"));
        let body_len = u32_at(28);
        if body_len as usize > MAX_BODY {
            return Err(WireError::TooLarge(body_len));
        }
        Ok(Self {
            kind,
            flags: u16_at(2),
            stream_id: u32_at(4),
            seq: u64_at(8),
            timestamp_ns: u64_at(16),
            chunk_index: u16_at(24),
            chunk_count: u16_at(26),
            body_len,
        })
    }
}

impl Message {
    #[must_use]
    pub fn new(kind: Kind, seq: u64, body: Vec<u8>) -> Self {
        Self {
            header: Header {
                kind,
                flags: 0,
                stream_id: 0,
                seq,
                timestamp_ns: 0,
                chunk_index: 0,
                chunk_count: 1,
                body_len: u32::try_from(body.len()).expect("body within u32"),
            },
            body,
        }
    }

    pub fn write_to<W: Write>(&self, w: &mut W) -> Result<(), WireError> {
        let mut header = self.header;
        header.body_len = u32::try_from(self.body.len()).map_err(|_| WireError::TooLarge(u32::MAX))?;
        w.write_all(&header.encode())?;
        w.write_all(&self.body)?;
        Ok(())
    }

    /// Reads one message. An `Ok(None)` is a clean end of stream at a message
    /// boundary; a partial header or body is an error.
    pub fn read_from<R: Read>(r: &mut R) -> Result<Option<Self>, WireError> {
        let mut head = [0u8; HEADER_LEN];
        match r.read(&mut head)? {
            0 => return Ok(None),
            n if n < HEADER_LEN => r.read_exact(&mut head[n..]).map_err(|_| WireError::Truncated("header"))?,
            _ => {}
        }
        let header = Header::decode(&head)?;
        let mut body = vec![0u8; header.body_len as usize];
        r.read_exact(&mut body).map_err(|_| WireError::Truncated("body"))?;
        Ok(Some(Self { header, body }))
    }
}

// ---- frame bodies ---------------------------------------------------------

/// The 144-byte descriptor, field by field, little-endian. It is `repr(C)` and
/// pointer-free, so this is a stable serialisation rather than a memory dump.
#[must_use]
pub fn encode_descriptor(d: &FrameDescriptor) -> [u8; DESCRIPTOR_LEN] {
    let mut out = [0u8; DESCRIPTOR_LEN];
    let mut at = 0;
    let mut put64 = |v: u64| {
        out[at..at + 8].copy_from_slice(&v.to_le_bytes());
        at += 8;
    };
    for v in [
        d.cursor,
        d.sequence,
        d.timestamp_ns,
        d.config_generation,
        d.pool_id,
        d.payload_offset,
        d.payload_len,
        d.modifier,
        d.fence_id,
        d.fence_value,
        d.damage_base_sequence,
        d.producer_drop_count,
    ] {
        put64(v);
    }
    let mut put32 = |v: u32| {
        out[at..at + 4].copy_from_slice(&v.to_le_bytes());
        at += 4;
    };
    for v in [
        d.width,
        d.height,
        d.stride,
        d.pixel_format,
        d.slot_id,
        d.clock_domain,
        d.color_space,
        d.sync_kind,
        d.payload_kind,
        d.damage_kind,
        d.dropped_before_publish,
        d.flags,
    ] {
        put32(v);
    }
    debug_assert_eq!(at, DESCRIPTOR_LEN);
    out
}

pub fn decode_descriptor(bytes: &[u8]) -> Result<FrameDescriptor, WireError> {
    if bytes.len() < DESCRIPTOR_LEN {
        return Err(WireError::Truncated("descriptor"));
    }
    let u64_at = |i: usize| u64::from_le_bytes(bytes[i..i + 8].try_into().expect("8 bytes"));
    let u32_at = |i: usize| u32::from_le_bytes(bytes[i..i + 4].try_into().expect("4 bytes"));
    Ok(FrameDescriptor {
        cursor: u64_at(0),
        sequence: u64_at(8),
        timestamp_ns: u64_at(16),
        config_generation: u64_at(24),
        pool_id: u64_at(32),
        payload_offset: u64_at(40),
        payload_len: u64_at(48),
        modifier: u64_at(56),
        fence_id: u64_at(64),
        fence_value: u64_at(72),
        damage_base_sequence: u64_at(80),
        producer_drop_count: u64_at(88),
        width: u32_at(96),
        height: u32_at(100),
        stride: u32_at(104),
        pixel_format: u32_at(108),
        slot_id: u32_at(112),
        clock_domain: u32_at(116),
        color_space: u32_at(120),
        sync_kind: u32_at(124),
        payload_kind: u32_at(128),
        damage_kind: u32_at(132),
        dropped_before_publish: u32_at(136),
        flags: u32_at(140),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// An operation over the consumer-side canvas. Version 1 only emits `Video`
/// with a full-frame rect; the other kinds are reserved names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Decode `access_unit` with the stream's codec configuration and place the
    /// result at `rect`. `plane_mask` selects planes for dual-view schemes; 0
    /// means all planes.
    Video {
        rect: Rect,
        stream_id: u32,
        plane_mask: u32,
        access_unit: Vec<u8>,
    },
}

const OP_VIDEO: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameBody {
    pub descriptor: FrameDescriptor,
    pub ops: Vec<Op>,
}

impl FrameBody {
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(DESCRIPTOR_LEN + 2 + self.ops.iter().map(Op::encoded_len).sum::<usize>());
        out.extend_from_slice(&encode_descriptor(&self.descriptor));
        out.extend_from_slice(&u16::try_from(self.ops.len()).expect("op count").to_le_bytes());
        for op in &self.ops {
            op.encode_into(&mut out);
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        let descriptor = decode_descriptor(bytes)?;
        let mut at = DESCRIPTOR_LEN;
        let take = |at: &mut usize, n: usize| -> Result<&[u8], WireError> {
            let s = bytes.get(*at..*at + n).ok_or(WireError::Truncated("op"))?;
            *at += n;
            Ok(s)
        };
        let count = u16::from_le_bytes(take(&mut at, 2)?.try_into().expect("2 bytes"));
        let mut ops = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            let kind = take(&mut at, 1)?[0];
            let len = u32::from_le_bytes(take(&mut at, 4)?.try_into().expect("4 bytes")) as usize;
            let payload = take(&mut at, len)?;
            ops.push(Op::decode(kind, payload)?);
        }
        Ok(Self { descriptor, ops })
    }
}

impl Op {
    fn encoded_len(&self) -> usize {
        match self {
            Self::Video { access_unit, .. } => 1 + 4 + 16 + 4 + 4 + access_unit.len(),
        }
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Self::Video {
                rect,
                stream_id,
                plane_mask,
                access_unit,
            } => {
                out.push(OP_VIDEO);
                let len = u32::try_from(16 + 4 + 4 + access_unit.len()).expect("op length");
                out.extend_from_slice(&len.to_le_bytes());
                for v in [rect.x, rect.y, rect.width, rect.height, *stream_id, *plane_mask] {
                    out.extend_from_slice(&v.to_le_bytes());
                }
                out.extend_from_slice(access_unit);
            }
        }
    }

    fn decode(kind: u8, payload: &[u8]) -> Result<Self, WireError> {
        match kind {
            OP_VIDEO => {
                if payload.len() < 24 {
                    return Err(WireError::Truncated("video op"));
                }
                let u32_at = |i: usize| u32::from_le_bytes(payload[i..i + 4].try_into().expect("4 bytes"));
                Ok(Self::Video {
                    rect: Rect {
                        x: u32_at(0),
                        y: u32_at(4),
                        width: u32_at(8),
                        height: u32_at(12),
                    },
                    stream_id: u32_at(16),
                    plane_mask: u32_at(20),
                    access_unit: payload[24..].to_vec(),
                })
            }
            other => Err(WireError::OpKind(other)),
        }
    }
}

// ---- control bodies (JSON) -------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Egress,
    Ingress,
}

/// First message on the control connection in each direction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub role: Role,
    pub protocol_version: u8,
    pub capabilities: CodecCapabilities,
    #[serde(default)]
    pub chroma_policy: ChromaPolicy,
    /// The token the coordinator minted for this export; each half checks the
    /// other's against what it was given. Empty when the link is trusted.
    #[serde(default)]
    pub token: String,
}

/// Sent before every keyframe so late joiners and relays are self-contained.
/// Parameter sets are raw NAL units (no start codes, emulation prevention kept),
/// in codec order: VPS, SPS, PPS for HEVC; SPS, PPS for H.264.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecConfig {
    pub codec: Codec,
    pub chroma: Chroma,
    pub full_range: bool,
    pub bit_depth: u8,
    pub width: u32,
    pub height: u32,
    /// The profile string the encoder was created with, for status only.
    pub profile: String,
    /// CoreMedia / H.273 colour description as (primaries, transfer, matrix).
    pub colour: (u8, u8, u8),
    pub parameter_sets: Vec<Vec<u8>>,
}

pub type TargetMsg = Target;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockPing {
    pub t1: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockPong {
    pub t1: u64,
    pub t2: u64,
    pub t3: u64,
}

impl ClockPing {
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        self.t1.to_le_bytes().to_vec()
    }
    pub fn decode(b: &[u8]) -> Result<Self, WireError> {
        Ok(Self {
            t1: u64::from_le_bytes(b.get(0..8).ok_or(WireError::Truncated("ping"))?.try_into().expect("8 bytes")),
        })
    }
}

impl ClockPong {
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(24);
        v.extend_from_slice(&self.t1.to_le_bytes());
        v.extend_from_slice(&self.t2.to_le_bytes());
        v.extend_from_slice(&self.t3.to_le_bytes());
        v
    }
    pub fn decode(b: &[u8]) -> Result<Self, WireError> {
        if b.len() < 24 {
            return Err(WireError::Truncated("pong"));
        }
        let at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().expect("8 bytes"));
        Ok(Self {
            t1: at(0),
            t2: at(8),
            t3: at(16),
        })
    }
}

pub fn json_message<T: Serialize>(kind: Kind, seq: u64, value: &T) -> Result<Message, WireError> {
    Ok(Message::new(kind, seq, serde_json::to_vec(value)?))
}

pub fn json_body<'a, T: Deserialize<'a>>(m: &'a Message) -> Result<T, WireError> {
    Ok(serde_json::from_slice(&m.body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor() -> FrameDescriptor {
        FrameDescriptor {
            cursor: 1,
            sequence: 2,
            timestamp_ns: 3,
            config_generation: 4,
            pool_id: 5,
            payload_offset: 6,
            payload_len: 7,
            modifier: 8,
            fence_id: 9,
            fence_value: 10,
            damage_base_sequence: 11,
            producer_drop_count: 12,
            width: 13,
            height: 14,
            stride: 15,
            pixel_format: 16,
            slot_id: 17,
            clock_domain: 18,
            color_space: 19,
            sync_kind: 20,
            payload_kind: 21,
            damage_kind: 22,
            dropped_before_publish: 23,
            flags: 24,
        }
    }

    #[test]
    fn header_round_trips_and_is_32_bytes() {
        let h = Header {
            kind: Kind::Frame,
            flags: flags::KEYFRAME | flags::CONFIG_CHANGED,
            stream_id: 7,
            seq: 0x0102_0304_0506_0708,
            timestamp_ns: 99,
            chunk_index: 2,
            chunk_count: 3,
            body_len: 1234,
        };
        let bytes = h.encode();
        assert_eq!(bytes.len(), 32);
        assert_eq!(Header::decode(&bytes).unwrap(), h);
    }

    #[test]
    fn header_rejects_bad_version_and_kind() {
        let mut bytes = Header::encode(&Message::new(Kind::Hello, 0, vec![]).header);
        bytes[0] = 9;
        assert!(matches!(Header::decode(&bytes), Err(WireError::Version(9))));
        bytes[0] = VERSION;
        bytes[1] = 200;
        assert!(matches!(Header::decode(&bytes), Err(WireError::Kind(200))));
    }

    #[test]
    fn descriptor_round_trips_at_144_bytes() {
        let d = descriptor();
        let bytes = encode_descriptor(&d);
        assert_eq!(bytes.len(), 144);
        // matches the repr(C) layout the C ABI asserts: fence_value at 72, width at 96, flags at 140
        assert_eq!(u64::from_le_bytes(bytes[72..80].try_into().unwrap()), 10);
        assert_eq!(u32::from_le_bytes(bytes[96..100].try_into().unwrap()), 13);
        assert_eq!(u32::from_le_bytes(bytes[140..144].try_into().unwrap()), 24);
        assert_eq!(decode_descriptor(&bytes).unwrap(), d);
    }

    #[test]
    fn frame_body_round_trips_one_video_op() {
        let body = FrameBody {
            descriptor: descriptor(),
            ops: vec![Op::Video {
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 13,
                    height: 14,
                },
                stream_id: 1,
                plane_mask: 0,
                access_unit: vec![0, 0, 0, 1, 0x40, 1, 2, 3],
            }],
        };
        let bytes = body.encode();
        assert_eq!(FrameBody::decode(&bytes).unwrap(), body);
        assert!(matches!(FrameBody::decode(&bytes[..150]), Err(WireError::Truncated(_))));
    }

    #[test]
    fn messages_stream_through_a_byte_pipe() {
        let a = Message::new(Kind::Hello, 1, b"{}".to_vec());
        let mut b = Message::new(Kind::Frame, 2, vec![7; 300]);
        b.header.flags = flags::KEYFRAME;
        b.header.timestamp_ns = 42;
        let mut buf = Vec::new();
        a.write_to(&mut buf).unwrap();
        b.write_to(&mut buf).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        assert_eq!(Message::read_from(&mut cursor).unwrap().unwrap(), a);
        assert_eq!(Message::read_from(&mut cursor).unwrap().unwrap(), b);
        assert!(Message::read_from(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn truncated_body_is_an_error_not_an_end() {
        let m = Message::new(Kind::Frame, 1, vec![1; 10]);
        let mut buf = Vec::new();
        m.write_to(&mut buf).unwrap();
        buf.truncate(HEADER_LEN + 5);
        let mut cursor = std::io::Cursor::new(buf);
        assert!(matches!(Message::read_from(&mut cursor), Err(WireError::Truncated("body"))));
    }

    #[test]
    fn control_bodies_round_trip_as_json() {
        let hello = Hello {
            role: Role::Egress,
            protocol_version: VERSION,
            capabilities: CodecCapabilities {
                hevc_444_hardware: true,
                ..Default::default()
            },
            chroma_policy: ChromaPolicy::Prefer444,
            token: "abc".into(),
        };
        let m = json_message(Kind::Hello, 0, &hello).unwrap();
        assert_eq!(json_body::<Hello>(&m).unwrap(), hello);
        let cfg = CodecConfig {
            codec: Codec::Hevc,
            chroma: Chroma::Full,
            full_range: false,
            bit_depth: 8,
            width: 1920,
            height: 1080,
            profile: "HEVC_Main444_AutoLevel".into(),
            colour: (1, 13, 1),
            parameter_sets: vec![vec![0x40, 1], vec![0x42, 1], vec![0x44, 1]],
        };
        let m = json_message(Kind::CodecConfig, 0, &cfg).unwrap();
        assert_eq!(json_body::<CodecConfig>(&m).unwrap(), cfg);
    }

    #[test]
    fn clock_bodies_round_trip() {
        let ping = ClockPing { t1: 5 };
        assert_eq!(ClockPing::decode(&ping.encode()).unwrap(), ping);
        let pong = ClockPong { t1: 5, t2: 6, t3: 7 };
        assert_eq!(ClockPong::decode(&pong.encode()).unwrap(), pong);
        assert!(ClockPong::decode(&[0; 3]).is_err());
    }
}
