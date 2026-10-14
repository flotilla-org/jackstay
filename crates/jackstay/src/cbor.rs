//! The one CBOR codec for Jackstay setup messages, enforcing the stack profile
//! of flotilla ADR 0063 (see docs/design/setup-encoding.md).
//!
//! Every setup message is encoded and decoded through this module, never by
//! calling a CBOR crate directly. Encoding is deterministic (RFC 8949 §4.2):
//! shortest heads, shortest float form, definite lengths, text map keys sorted
//! bytewise by their encoding, and an integral float encoded as an integer.
//! Decoding is strict: anything outside that form is rejected with a specific
//! [`Error`] rather than normalized, so every accepted value has exactly one
//! byte form. Unknown map keys are ignored by typed decoding; unknown message
//! types are rejected.
//!
//! # Why a small hand-written codec
//!
//! The profile is mostly about what a reader refuses, and the readers of the
//! serde CBOR crates do not enforce it. `ciborium` (Apache-2.0) accepts
//! non-shortest heads, indefinite lengths and duplicate map keys, writes struct
//! fields in declaration order and has no numeric reduction, so enforcing the
//! profile around it needs a full validating pass and a canonicalizing
//! re-encode: the bulk of a codec, plus `ciborium-io`, `ciborium-ll` and
//! `half`. `minicbor` is licensed BlueOak-1.0.0, outside the workspace's
//! MIT/Apache-2.0 set. The data model needed here (integers, floats, byte and
//! text strings, arrays, text-keyed maps, booleans, null) is small, so the
//! codec is written against it directly with no new dependency. Serde
//! integration goes through [`Value`]: typed messages serialize to a value that
//! is then encoded, and decode from a value that has already passed every
//! check.
//!
//! # Limits
//!
//! [`Limits`] bound message size, nesting depth, elements per array or map and
//! the length of any byte string. The decoder checks a declared length or count
//! against its limit and against the bytes actually remaining before using it,
//! and never reserves capacity from a declared length, so a short message that
//! claims a huge array fails without allocating. [`Framing`] checks a frame's
//! length prefix against the message limit before allocating its body.

mod de;
mod decode;
mod encode;
mod float;
mod frame;
mod ser;
mod value;

use std::io;

use serde::{Serialize, de::DeserializeOwned};

pub use self::{
    frame::{Framing, HEADER_LEN, MAGIC_LEN},
    value::{Bytes, Value},
};

/// The ADR 0063 defaults for a protocol that sets no limits of its own.
pub const DEFAULT_LIMITS: Limits = Limits {
    max_message: 1024 * 1024,
    max_depth: 64,
    max_elements: 65_536,
    max_byte_string: 1024 * 1024,
};

/// The largest opaque descriptor a Jackstay setup message may carry.
pub const SETUP_MAX_DESCRIPTOR: usize = 4 * 1024;

/// The Jackstay setup profile: 64 KiB per message, and no byte string (where
/// opaque descriptors travel) longer than [`SETUP_MAX_DESCRIPTOR`].
pub const SETUP_LIMITS: Limits = Limits {
    max_message: 64 * 1024,
    max_depth: 64,
    max_elements: 65_536,
    max_byte_string: SETUP_MAX_DESCRIPTOR,
};

/// Bounds a protocol places on one encoded message. Each limit is inclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Encoded bytes in one message body, excluding any frame header.
    pub max_message: usize,
    /// Nested arrays and maps. The outermost container is at depth 1. The
    /// decoder recurses once per level, so keep this small.
    pub max_depth: usize,
    /// Elements in one array, or entries in one map.
    pub max_elements: usize,
    /// Bytes in any one byte string.
    pub max_byte_string: usize,
}

impl Default for Limits {
    fn default() -> Self {
        DEFAULT_LIMITS
    }
}

/// A typed message: one CBOR map whose `type` text field names its kind.
///
/// A `#[serde(tag = "type")]` enum is the usual shape. Serde buffers such an
/// enum's fields before choosing the variant. That buffer cannot hold an
/// integer below `i64::MIN`, and reads an integer into a float field without
/// the exactness check a direct struct field gets, so prefer a struct per type
/// where either matters.
pub trait Message: Serialize + DeserializeOwned {
    /// Every `type` this message accepts. Any other type is rejected.
    const TYPES: &'static [&'static str];
}

/// Why a value could not be encoded or decoded under the profile.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("input ends inside a data item")]
    Truncated,
    #[error("bytes follow the data item")]
    TrailingBytes,
    #[error("reserved additional information {0}")]
    Reserved(u8),
    #[error("indefinite length")]
    IndefiniteLength,
    #[error("break code outside an indefinite-length item")]
    UnexpectedBreak,
    #[error("integer not in its shortest form")]
    NonShortestInteger,
    #[error("length not in its shortest form")]
    NonShortestLength,
    #[error("float not in its shortest form")]
    NonShortestFloat,
    #[error("integral value encoded as a float")]
    IntegralFloat,
    #[error("NaN")]
    Nan,
    #[error("tag {0} is not allowed")]
    Tag(u64),
    #[error("unsupported simple value")]
    UnsupportedSimple,
    #[error("map key is not a text string")]
    NonTextKey,
    #[error("duplicate map key {0:?}")]
    DuplicateKey(String),
    #[error("map keys out of order")]
    UnsortedKeys,
    #[error("text string is not UTF-8")]
    InvalidUtf8,
    #[error("integer outside the CBOR range")]
    IntegerOutOfRange,
    #[error("nesting deeper than {limit}")]
    DepthLimit { limit: usize },
    #[error("{count} elements, over the limit of {limit}")]
    ElementLimit { count: u64, limit: usize },
    #[error("message of {len} bytes, over the limit of {limit}")]
    MessageLimit { len: u64, limit: usize },
    #[error("byte string of {len} bytes, over the limit of {limit}")]
    ByteStringLimit { len: u64, limit: usize },
    #[error("message is not a map")]
    NotAMap,
    #[error("message has no type field")]
    MissingType,
    #[error("message type is not a text string")]
    TypeNotText,
    #[error("unknown message type {0:?}")]
    UnknownType(String),
    #[error("frame magic does not match the protocol")]
    BadMagic,
    #[error("{0}")]
    Schema(String),
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
}

impl Error {
    /// A stable name for the error, as used by the conformance corpus.
    pub fn kind(&self) -> &'static str {
        match self {
            Error::Truncated => "truncated",
            Error::TrailingBytes => "trailing-bytes",
            Error::Reserved(_) => "reserved",
            Error::IndefiniteLength => "indefinite-length",
            Error::UnexpectedBreak => "unexpected-break",
            Error::NonShortestInteger => "non-shortest-integer",
            Error::NonShortestLength => "non-shortest-length",
            Error::NonShortestFloat => "non-shortest-float",
            Error::IntegralFloat => "integral-float",
            Error::Nan => "nan",
            Error::Tag(_) => "tag",
            Error::UnsupportedSimple => "unsupported-simple",
            Error::NonTextKey => "non-text-key",
            Error::DuplicateKey(_) => "duplicate-key",
            Error::UnsortedKeys => "unsorted-keys",
            Error::InvalidUtf8 => "invalid-utf8",
            Error::IntegerOutOfRange => "integer-out-of-range",
            Error::DepthLimit { .. } => "depth-limit",
            Error::ElementLimit { .. } => "element-limit",
            Error::MessageLimit { .. } => "message-limit",
            Error::ByteStringLimit { .. } => "byte-string-limit",
            Error::NotAMap => "not-a-map",
            Error::MissingType => "missing-type",
            Error::TypeNotText => "type-not-text",
            Error::UnknownType(_) => "unknown-type",
            Error::BadMagic => "bad-magic",
            Error::Schema(_) => "schema",
            Error::Io(_) => "io",
        }
    }
}

impl serde::ser::Error for Error {
    fn custom<T: std::fmt::Display>(msg: T) -> Self {
        Error::Schema(msg.to_string())
    }
}

impl serde::de::Error for Error {
    fn custom<T: std::fmt::Display>(msg: T) -> Self {
        Error::Schema(msg.to_string())
    }
}

/// Encode a value deterministically, within `limits`.
pub fn encode_value(value: &Value, limits: &Limits) -> Result<Vec<u8>, Error> {
    encode::encode(value, limits)
}

/// Decode exactly one data item, rejecting anything outside the profile.
pub fn decode_value(bytes: &[u8], limits: &Limits) -> Result<Value, Error> {
    decode::decode(bytes, limits)
}

/// The [`Value`] a serializable type maps to.
pub fn to_value<T: Serialize + ?Sized>(value: &T) -> Result<Value, Error> {
    value.serialize(ser::ValueSerializer)
}

/// A deserializable type from a [`Value`]. Unknown map keys are ignored unless
/// the type itself denies them.
pub fn from_value<T: DeserializeOwned>(value: Value) -> Result<T, Error> {
    T::deserialize(value)
}

/// Serialize and encode any value. Use [`encode_message`] for setup messages.
pub fn to_vec<T: Serialize + ?Sized>(value: &T, limits: &Limits) -> Result<Vec<u8>, Error> {
    encode_value(&to_value(value)?, limits)
}

/// Decode and deserialize any value. Use [`decode_message`] for setup messages.
pub fn from_slice<T: DeserializeOwned>(bytes: &[u8], limits: &Limits) -> Result<T, Error> {
    from_value(decode_value(bytes, limits)?)
}

/// Encode a message body, checking that it is a map of a known `type`.
pub fn encode_message<M: Message>(message: &M, limits: &Limits) -> Result<Vec<u8>, Error> {
    let value = to_value(message)?;
    message_type(&value, M::TYPES)?;
    encode_value(&value, limits)
}

/// Decode a message body: the profile first, then its `type`, then its fields.
pub fn decode_message<M: Message>(bytes: &[u8], limits: &Limits) -> Result<M, Error> {
    let value = decode_value(bytes, limits)?;
    message_type(&value, M::TYPES)?;
    from_value(value)
}

/// The message's `type`, if the value is a map whose `type` is one of `types`.
pub fn message_type<'a>(value: &'a Value, types: &[&str]) -> Result<&'a str, Error> {
    let Value::Map(entries) = value else {
        return Err(Error::NotAMap);
    };
    let (_, kind) = entries.iter().find(|(key, _)| key == "type").ok_or(Error::MissingType)?;
    let Value::Text(kind) = kind else {
        return Err(Error::TypeNotText);
    };
    if !types.contains(&kind.as_str()) {
        return Err(Error::UnknownType(kind.clone()));
    }
    Ok(kind)
}

#[cfg(test)]
mod tests;
