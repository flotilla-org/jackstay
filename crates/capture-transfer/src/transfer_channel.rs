use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum CaptureTransferRequest {
    Authorize {
        session_id: String,
        bearer_token: String,
    },
    LatestVideoFrame {
        session_id: String,
        track_id: u64,
    },
    AcquireVideoFrameByCursor {
        session_id: String,
        track_id: u64,
        producer_cursor: u64,
    },
    AcquireNextVideoFrame {
        session_id: String,
        track_id: u64,
        after_producer_cursor: u64,
    },
    ReleaseVideoFrame {
        lease_id: u64,
    },
}

impl fmt::Debug for CaptureTransferRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Authorize { session_id, .. } => f
                .debug_struct("Authorize")
                .field("session_id", session_id)
                .field("bearer_token", &"<redacted>")
                .finish(),
            Self::LatestVideoFrame { session_id, track_id } => f
                .debug_struct("LatestVideoFrame")
                .field("session_id", session_id)
                .field("track_id", track_id)
                .finish(),
            Self::AcquireVideoFrameByCursor {
                session_id,
                track_id,
                producer_cursor,
            } => f
                .debug_struct("AcquireVideoFrameByCursor")
                .field("session_id", session_id)
                .field("track_id", track_id)
                .field("producer_cursor", producer_cursor)
                .finish(),
            Self::AcquireNextVideoFrame {
                session_id,
                track_id,
                after_producer_cursor,
            } => f
                .debug_struct("AcquireNextVideoFrame")
                .field("session_id", session_id)
                .field("track_id", track_id)
                .field("after_producer_cursor", after_producer_cursor)
                .finish(),
            Self::ReleaseVideoFrame { lease_id } => f.debug_struct("ReleaseVideoFrame").field("lease_id", lease_id).finish(),
        }
    }
}

// Keep the enum variants flat so serde produces the channel JSON directly
// without boxing/flattening helpers on this pre-shared-memory control path.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum CaptureTransferMessage {
    RegisterVideoControlPage {
        session_id: String,
        track_id: u64,
        map_len: u64,
        consumer_id: u64,
        consumer_slot: u64,
    },
    RegisterCpuPool {
        session_id: String,
        track_id: u64,
        pool_id: u64,
        pool_generation: u64,
        payload_map_len: u64,
        slot_stride: u64,
        slot_count: u64,
    },
    VideoFrame {
        session_id: String,
        track_id: u64,
        lease_id: u64,
        producer_cursor: u64,
        sequence: u64,
        timestamp_ns: u64,
        width: u32,
        height: u32,
        stride: u32,
        pixel_format: String,
        pool_id: u64,
        slot_id: u64,
        slot_generation: u64,
        payload_offset: u64,
        payload_len: u64,
        payload_map_len: u64,
        clock_domain: String,
        color_space: String,
        sync_kind: String,
        damage_kind: String,
        damage_base_sequence: u64,
        dropped_before_publish: u64,
        producer_drop_count: u64,
        evicted_count: u64,
        consumer_skipped_count: u64,
        len: u64,
    },
    VideoFrameUnavailable {
        session_id: String,
        track_id: u64,
        after_producer_cursor: u64,
        oldest_available_cursor: u64,
        latest_available_cursor: u64,
        skipped_count: u64,
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::{CaptureTransferMessage, CaptureTransferRequest};

    #[test]
    fn request_messages_roundtrip_with_snake_case_ops() {
        let authorize = CaptureTransferRequest::Authorize {
            session_id: "session-1".to_string(),
            bearer_token: "pta_agent.secret".to_string(),
        };
        let authorize_json = serde_json::to_value(&authorize).unwrap();
        assert_eq!(authorize_json["op"], "authorize");
        assert_eq!(authorize_json["session_id"], "session-1");
        assert_eq!(authorize_json["bearer_token"], "pta_agent.secret");
        assert_eq!(serde_json::from_value::<CaptureTransferRequest>(authorize_json).unwrap(), authorize);

        let latest = CaptureTransferRequest::LatestVideoFrame {
            session_id: "session-1".to_string(),
            track_id: 7,
        };
        let latest_json = serde_json::to_value(&latest).unwrap();
        assert_eq!(latest_json["op"], "latest_video_frame");
        assert_eq!(latest_json["session_id"], "session-1");
        assert_eq!(latest_json["track_id"], 7);
        assert_eq!(serde_json::from_value::<CaptureTransferRequest>(latest_json).unwrap(), latest);

        let acquire = CaptureTransferRequest::AcquireVideoFrameByCursor {
            session_id: "session-1".to_string(),
            track_id: 7,
            producer_cursor: 99,
        };
        let acquire_json = serde_json::to_value(&acquire).unwrap();
        assert_eq!(acquire_json["op"], "acquire_video_frame_by_cursor");
        assert_eq!(acquire_json["session_id"], "session-1");
        assert_eq!(acquire_json["track_id"], 7);
        assert_eq!(acquire_json["producer_cursor"], 99);
        assert_eq!(serde_json::from_value::<CaptureTransferRequest>(acquire_json).unwrap(), acquire);

        let next = CaptureTransferRequest::AcquireNextVideoFrame {
            session_id: "session-1".to_string(),
            track_id: 7,
            after_producer_cursor: 42,
        };
        let next_json = serde_json::to_value(&next).unwrap();
        assert_eq!(next_json["op"], "acquire_next_video_frame");
        assert_eq!(next_json["session_id"], "session-1");
        assert_eq!(next_json["track_id"], 7);
        assert_eq!(next_json["after_producer_cursor"], 42);
        assert_eq!(serde_json::from_value::<CaptureTransferRequest>(next_json).unwrap(), next);

        let release = CaptureTransferRequest::ReleaseVideoFrame { lease_id: 42 };
        let release_json = serde_json::to_value(&release).unwrap();
        assert_eq!(release_json["op"], "release_video_frame");
        assert_eq!(release_json["lease_id"], 42);
        assert_eq!(serde_json::from_value::<CaptureTransferRequest>(release_json).unwrap(), release);
    }

    #[test]
    fn request_debug_redacts_bearer_token() {
        let request = CaptureTransferRequest::Authorize {
            session_id: "session-1".to_string(),
            bearer_token: "pta_agent.secret".to_string(),
        };

        let output = format!("{request:?}");
        assert!(output.contains("<redacted>"));
        assert!(!output.contains("pta_agent.secret"));
    }

    #[test]
    fn server_messages_roundtrip_with_producer_cursor() {
        let control = CaptureTransferMessage::RegisterVideoControlPage {
            session_id: "session-1".to_string(),
            track_id: 7,
            map_len: 4096,
            consumer_id: 12,
            consumer_slot: 1,
        };
        let control_json = serde_json::to_value(&control).unwrap();
        assert_eq!(control_json["op"], "register_video_control_page");
        assert_eq!(control_json["map_len"], 4096);
        assert_eq!(control_json["consumer_id"], 12);
        assert_eq!(control_json["consumer_slot"], 1);
        assert_eq!(serde_json::from_value::<CaptureTransferMessage>(control_json).unwrap(), control);

        let register = CaptureTransferMessage::RegisterCpuPool {
            session_id: "session-1".to_string(),
            track_id: 7,
            pool_id: 3,
            pool_generation: 5,
            payload_map_len: 4096,
            slot_stride: 1024,
            slot_count: 4,
        };
        let register_json = serde_json::to_value(&register).unwrap();
        assert_eq!(register_json["op"], "register_cpu_pool");
        assert_eq!(register_json["pool_generation"], 5);
        assert_eq!(serde_json::from_value::<CaptureTransferMessage>(register_json).unwrap(), register);

        let frame = CaptureTransferMessage::VideoFrame {
            session_id: "session-1".to_string(),
            track_id: 7,
            lease_id: 42,
            producer_cursor: 99,
            sequence: 100,
            timestamp_ns: 1_000,
            width: 2,
            height: 1,
            stride: 8,
            pixel_format: "bgra8_unorm".to_string(),
            pool_id: 3,
            slot_id: 1,
            slot_generation: 5,
            payload_offset: 1024,
            payload_len: 8,
            payload_map_len: 4096,
            clock_domain: "media_time".to_string(),
            color_space: "srgb".to_string(),
            sync_kind: "cpu_copy_complete".to_string(),
            damage_kind: "full_frame".to_string(),
            damage_base_sequence: 100,
            dropped_before_publish: 0,
            producer_drop_count: 0,
            evicted_count: 0,
            consumer_skipped_count: 2,
            len: 8,
        };
        let frame_json = serde_json::to_value(&frame).unwrap();
        assert_eq!(frame_json["op"], "video_frame");
        assert_eq!(frame_json["producer_cursor"], 99);
        assert_eq!(serde_json::from_value::<CaptureTransferMessage>(frame_json).unwrap(), frame);

        let unavailable = CaptureTransferMessage::VideoFrameUnavailable {
            session_id: "session-1".to_string(),
            track_id: 7,
            after_producer_cursor: 42,
            oldest_available_cursor: 48,
            latest_available_cursor: 57,
            skipped_count: 5,
            reason: "lapped".to_string(),
        };
        let unavailable_json = serde_json::to_value(&unavailable).unwrap();
        assert_eq!(unavailable_json["op"], "video_frame_unavailable");
        assert_eq!(unavailable_json["after_producer_cursor"], 42);
        assert_eq!(unavailable_json["oldest_available_cursor"], 48);
        assert_eq!(unavailable_json["latest_available_cursor"], 57);
        assert_eq!(unavailable_json["skipped_count"], 5);
        assert_eq!(
            serde_json::from_value::<CaptureTransferMessage>(unavailable_json).unwrap(),
            unavailable
        );
    }
}
