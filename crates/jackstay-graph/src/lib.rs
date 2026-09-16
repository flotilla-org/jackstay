//! Graph-manager vocabulary for Jackstay publications.
//!
//! A *publication* is a running stream with a local endpoint. A *graph manager*
//! (portholed where a desktop exists, a slim daemon in a container) starts
//! producers, connects consumers and inserts transforms that consume one
//! publication and produce another. An *export* is the graph-manager operation
//! that creates a cross-host edge: an egress half consumes a publication on one
//! host, an ingress half produces a new publication on the other whose source
//! identity is the original source. See
//! `docs/design/publication-registry-and-graphs.md`.
//!
//! This crate holds only the shared types and decisions those operations need.
//! It has no daemon, no transport and no dependency on a host.

use serde::{Deserialize, Serialize};

pub mod export;
#[cfg(target_os = "macos")]
pub mod launchd;

/// How an export treats the absence of hardware 4:4:4 on either end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChromaPolicy {
    /// Refuse the export unless both ends encode and decode 4:4:4 in hardware.
    Require444,
    /// Use 4:4:4 when both ends can, otherwise fall back and record it.
    #[default]
    Prefer444,
    /// Take whatever is cheapest.
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    Hevc,
    H264,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Chroma {
    /// 4:2:0.
    Subsampled,
    /// 4:4:4.
    Full,
}

/// What one end can do, as reported by its own capability probe at session
/// start. Every field is "verified by creating a session", never assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CodecCapabilities {
    pub hevc_444_hardware: bool,
    pub hevc_420_hardware: bool,
    pub h264_444_hardware: bool,
    pub h264_420_hardware: bool,
}

/// The codec an export settled on, with the reason recorded for status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecDecision {
    pub codec: Codec,
    pub chroma: Chroma,
    pub hardware: bool,
    pub reason: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecisionError {
    #[error("policy requires hardware 4:4:4 on both ends; egress {egress:?}, ingress {ingress:?}")]
    Require444Unmet {
        egress: CodecCapabilities,
        ingress: CodecCapabilities,
    },
    #[error("no codec is available in hardware on both ends; egress {egress:?}, ingress {ingress:?}")]
    NoCommonCodec {
        egress: CodecCapabilities,
        ingress: CodecCapabilities,
    },
}

/// Picks the codec for an export. HEVC is preferred over H.264 at equal chroma
/// because HEVC carries reorder-zero in its SPS and measured smaller; 4:4:4 is
/// preferred over 4:2:0 because chroma fidelity on text cannot be bought with
/// bitrate. Software paths are not offered here: an end that lacks hardware
/// simply does not set the flag.
pub fn decide(policy: ChromaPolicy, egress: CodecCapabilities, ingress: CodecCapabilities) -> Result<CodecDecision, DecisionError> {
    let both = |a: bool, b: bool| a && b;
    let hevc_444 = both(egress.hevc_444_hardware, ingress.hevc_444_hardware);
    let h264_444 = both(egress.h264_444_hardware, ingress.h264_444_hardware);
    let hevc_420 = both(egress.hevc_420_hardware, ingress.hevc_420_hardware);
    let h264_420 = both(egress.h264_420_hardware, ingress.h264_420_hardware);
    let pick = |codec, chroma, reason: &str| CodecDecision {
        codec,
        chroma,
        hardware: true,
        reason: reason.to_owned(),
    };
    if hevc_444 {
        return Ok(pick(Codec::Hevc, Chroma::Full, "hardware HEVC 4:4:4 on both ends"));
    }
    if h264_444 {
        return Ok(pick(
            Codec::H264,
            Chroma::Full,
            "hardware H.264 4:4:4 on both ends; HEVC 4:4:4 missing on one end",
        ));
    }
    if policy == ChromaPolicy::Require444 {
        return Err(DecisionError::Require444Unmet { egress, ingress });
    }
    if hevc_420 {
        return Ok(pick(
            Codec::Hevc,
            Chroma::Subsampled,
            "no hardware 4:4:4 on both ends; policy allows 4:2:0",
        ));
    }
    if h264_420 {
        return Ok(pick(
            Codec::H264,
            Chroma::Subsampled,
            "no hardware 4:4:4 or HEVC on both ends; policy allows 4:2:0",
        ));
    }
    Err(DecisionError::NoCommonCodec { egress, ingress })
}

/// What the consumer side wants. Coalesced: a newer target replaces an older
/// one, nothing is queued. Size requests are wishes; the producer-side
/// coordinator decides whether to change the capture output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Target {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub max_fps: Option<u32>,
    /// Frames the consumer wants to be able to hold at once.
    pub holding: Option<u32>,
}

/// The two identities an export keeps apart: what is being shown, and the
/// running stream that shows it right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identities {
    /// Stable identity of the thing captured (a surface, an app), owned by the
    /// producer-side coordinator.
    pub source: String,
    /// Identity of the running publication this export consumes. Changes when
    /// the producer restarts.
    pub publication: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Carrier {
    /// Two byte streams (media, control) over sockets the coordinator forwards.
    StreamLocal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportRequest {
    pub publication: String,
    pub carrier: Carrier,
    #[serde(default)]
    pub chroma: ChromaPolicy,
    #[serde(default)]
    pub target: Target,
}

/// A single-use capability handed to a bridge half by the coordinator that
/// spawned it. 32 bytes from the OS random source, hex encoded.
pub fn mint_token() -> std::io::Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(hevc444: bool, h264_444: bool, hevc420: bool, h264_420: bool) -> CodecCapabilities {
        CodecCapabilities {
            hevc_444_hardware: hevc444,
            hevc_420_hardware: hevc420,
            h264_444_hardware: h264_444,
            h264_420_hardware: h264_420,
        }
    }

    #[test]
    fn prefers_hevc_444_when_both_ends_have_it() {
        let d = decide(ChromaPolicy::Prefer444, caps(true, true, true, true), caps(true, true, true, true)).unwrap();
        assert_eq!((d.codec, d.chroma), (Codec::Hevc, Chroma::Full));
    }

    #[test]
    fn falls_back_to_h264_444_then_hevc_420() {
        let d = decide(ChromaPolicy::Prefer444, caps(false, true, true, true), caps(true, true, true, true)).unwrap();
        assert_eq!((d.codec, d.chroma), (Codec::H264, Chroma::Full));
        let d = decide(
            ChromaPolicy::Prefer444,
            caps(false, false, true, true),
            caps(true, true, true, true),
        )
        .unwrap();
        assert_eq!((d.codec, d.chroma), (Codec::Hevc, Chroma::Subsampled));
    }

    #[test]
    fn require_444_refuses_and_records_both_ends() {
        let e = caps(false, false, true, true);
        let i = caps(true, true, true, true);
        assert_eq!(
            decide(ChromaPolicy::Require444, e, i),
            Err(DecisionError::Require444Unmet { egress: e, ingress: i })
        );
    }

    #[test]
    fn no_common_hardware_is_an_error() {
        assert!(matches!(
            decide(ChromaPolicy::Any, caps(true, false, true, false), caps(false, true, false, true)),
            Err(DecisionError::NoCommonCodec { .. })
        ));
    }

    #[test]
    fn tokens_are_distinct_hex() {
        let a = mint_token().unwrap();
        let b = mint_token().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
