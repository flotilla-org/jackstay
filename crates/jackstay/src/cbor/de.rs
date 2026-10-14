//! Serde deserialization from a [`Value`] that already passed the profile.
use serde::de::{self, IntoDeserializer, Visitor};

use super::{Error, Value, float};

impl<'de> de::Deserializer<'de> for Value {
    type Error = Error;

    fn is_human_readable(&self) -> bool {
        false
    }

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self {
            Value::Null => visitor.visit_unit(),
            Value::Bool(v) => visitor.visit_bool(v),
            Value::Integer(n) => {
                if let Ok(n) = u64::try_from(n) {
                    visitor.visit_u64(n)
                } else if let Ok(n) = i64::try_from(n) {
                    visitor.visit_i64(n)
                } else {
                    visitor.visit_i128(n)
                }
            }
            Value::Float(f) => visitor.visit_f64(f),
            Value::Bytes(v) => visitor.visit_byte_buf(v),
            Value::Text(v) => visitor.visit_string(v),
            Value::Array(items) => {
                let len = items.len();
                let mut seq = Seq(items.into_iter());
                let value = visitor.visit_seq(&mut seq)?;
                if seq.0.len() != 0 {
                    return Err(de::Error::invalid_length(len, &"fewer elements"));
                }
                Ok(value)
            }
            Value::Map(entries) => {
                let len = entries.len();
                let mut map = Map {
                    entries: entries.into_iter(),
                    value: None,
                };
                let value = visitor.visit_map(&mut map)?;
                if map.entries.len() != 0 {
                    return Err(de::Error::invalid_length(len, &"fewer entries"));
                }
                Ok(value)
            }
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self {
            Value::Null => visitor.visit_none(),
            value => visitor.visit_some(value),
        }
    }

    /// An integer stands for an integral float only if it is exactly one.
    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self {
            Value::Integer(n) => visitor.visit_f64(exact_float(n)?),
            value => value.deserialize_any(visitor),
        }
    }

    /// Only a value binary32 holds exactly, so no peer's f32 is rounded.
    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let f = match self {
            Value::Integer(n) => exact_float(n)?,
            Value::Float(f) => f,
            value => return value.deserialize_any(visitor),
        };
        if !float::fits_f32(f) {
            return Err(Error::Schema(format!("{f} is not exactly an f32")));
        }
        visitor.visit_f32(f as f32)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(self, _: &'static str, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_newtype_struct(self)
    }

    /// Externally tagged: a unit variant is its name; any other variant is a
    /// one-entry map from its name to its content.
    fn deserialize_enum<V: Visitor<'de>>(self, _: &'static str, _: &'static [&'static str], visitor: V) -> Result<V::Value, Error> {
        match self {
            Value::Text(variant) => visitor.visit_enum(variant.into_deserializer()),
            Value::Map(mut entries) if entries.len() == 1 => {
                let (variant, value) = entries.pop().unwrap();
                visitor.visit_enum(Enum { variant, value })
            }
            _ => Err(Error::Schema("expected an enum variant".into())),
        }
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 char str string
        bytes byte_buf unit unit_struct seq tuple tuple_struct map struct
        identifier ignored_any
    }
}

fn exact_float(n: i128) -> Result<f64, Error> {
    let f = n as f64;
    if float::integral(f) != Some(n) {
        return Err(Error::Schema(format!("{n} is not exactly a float")));
    }
    Ok(f)
}

impl IntoDeserializer<'_, Error> for Value {
    type Deserializer = Self;
    fn into_deserializer(self) -> Self {
        self
    }
}

struct Seq(std::vec::IntoIter<Value>);

impl<'de> de::SeqAccess<'de> for Seq {
    type Error = Error;
    fn next_element_seed<T: de::DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>, Error> {
        self.0.next().map(|value| seed.deserialize(value)).transpose()
    }
    fn size_hint(&self) -> Option<usize> {
        Some(self.0.len())
    }
}

struct Map {
    entries: std::vec::IntoIter<(String, Value)>,
    value: Option<Value>,
}

impl<'de> de::MapAccess<'de> for Map {
    type Error = Error;
    fn next_key_seed<K: de::DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, Error> {
        let Some((key, value)) = self.entries.next() else {
            return Ok(None);
        };
        self.value = Some(value);
        seed.deserialize(Value::Text(key)).map(Some)
    }
    fn next_value_seed<V: de::DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Error> {
        let value = self.value.take().ok_or_else(|| Error::Schema("map value without a key".into()))?;
        seed.deserialize(value)
    }
    fn size_hint(&self) -> Option<usize> {
        Some(self.entries.len())
    }
}

struct Enum {
    variant: String,
    value: Value,
}

impl<'de> de::EnumAccess<'de> for Enum {
    type Error = Error;
    type Variant = Value;
    fn variant_seed<V: de::DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Value), Error> {
        Ok((seed.deserialize(Value::Text(self.variant))?, self.value))
    }
}

impl<'de> de::VariantAccess<'de> for Value {
    type Error = Error;
    fn unit_variant(self) -> Result<(), Error> {
        Err(Error::Schema("unit variant with content".into()))
    }
    fn newtype_variant_seed<T: de::DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, Error> {
        seed.deserialize(self)
    }
    fn tuple_variant<V: Visitor<'de>>(self, _: usize, visitor: V) -> Result<V::Value, Error> {
        de::Deserializer::deserialize_any(self, visitor)
    }
    fn struct_variant<V: Visitor<'de>>(self, _: &'static [&'static str], visitor: V) -> Result<V::Value, Error> {
        de::Deserializer::deserialize_any(self, visitor)
    }
}
