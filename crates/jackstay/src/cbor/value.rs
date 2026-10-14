//! The profile's data model.
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// One data item in the profile's data model.
///
/// Decoding yields maps in canonical key order. Encoding sorts map entries
/// itself and rejects duplicates. A `Float` holding an integral value within
/// the integer range encodes as an integer (so decodes back as `Integer`);
/// `-0.0` and the infinities stay floats.
#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Bool(bool),
    /// An integer in `-2^64 ..= 2^64 - 1`; encoding rejects anything wider.
    Integer(i128),
    /// Any float but NaN, which encoding rejects.
    Float(f64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Value>),
    Map(Vec<(String, Value)>),
}

/// Floats compare by bit pattern, so `-0.0` and `0.0` differ.
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Integer(a), Value::Integer(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
            (Value::Bytes(a), Value::Bytes(b)) => a == b,
            (Value::Text(a), Value::Text(b)) => a == b,
            (Value::Array(a), Value::Array(b)) => a == b,
            (Value::Map(a), Value::Map(b)) => a == b,
            _ => false,
        }
    }
}

/// A byte string field. Serde maps a plain `Vec<u8>` to an array of integers;
/// wrap opaque bytes, such as descriptors, in this instead.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Bytes(pub Vec<u8>);

impl Serialize for Bytes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl de::Visitor<'_> for Visitor {
            type Value = Bytes;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a byte string")
            }
            fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Bytes, E> {
                Ok(Bytes(v.to_vec()))
            }
            fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Bytes, E> {
                Ok(Bytes(v))
            }
        }
        deserializer.deserialize_byte_buf(Visitor)
    }
}
