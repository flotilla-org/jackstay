//! Serde serialization into a [`Value`].
use serde::ser::{self, Serialize};

use super::{
    Error, Value,
    encode::{INTEGER_MAX, INTEGER_MIN},
};

pub(super) struct ValueSerializer;

fn integer(n: i128) -> Result<Value, Error> {
    if (INTEGER_MIN..=INTEGER_MAX).contains(&n) {
        Ok(Value::Integer(n))
    } else {
        Err(Error::IntegerOutOfRange)
    }
}

impl ser::Serializer for ValueSerializer {
    type Ok = Value;
    type Error = Error;
    type SerializeSeq = Seq;
    type SerializeTuple = Seq;
    type SerializeTupleStruct = Seq;
    type SerializeTupleVariant = Variant<Seq>;
    type SerializeMap = Map;
    type SerializeStruct = Map;
    type SerializeStructVariant = Variant<Map>;

    fn is_human_readable(&self) -> bool {
        false
    }

    fn serialize_bool(self, v: bool) -> Result<Value, Error> {
        Ok(Value::Bool(v))
    }
    fn serialize_i8(self, v: i8) -> Result<Value, Error> {
        integer(v.into())
    }
    fn serialize_i16(self, v: i16) -> Result<Value, Error> {
        integer(v.into())
    }
    fn serialize_i32(self, v: i32) -> Result<Value, Error> {
        integer(v.into())
    }
    fn serialize_i64(self, v: i64) -> Result<Value, Error> {
        integer(v.into())
    }
    fn serialize_i128(self, v: i128) -> Result<Value, Error> {
        integer(v)
    }
    fn serialize_u8(self, v: u8) -> Result<Value, Error> {
        integer(v.into())
    }
    fn serialize_u16(self, v: u16) -> Result<Value, Error> {
        integer(v.into())
    }
    fn serialize_u32(self, v: u32) -> Result<Value, Error> {
        integer(v.into())
    }
    fn serialize_u64(self, v: u64) -> Result<Value, Error> {
        integer(v.into())
    }
    fn serialize_u128(self, v: u128) -> Result<Value, Error> {
        integer(i128::try_from(v).map_err(|_| Error::IntegerOutOfRange)?)
    }
    fn serialize_f32(self, v: f32) -> Result<Value, Error> {
        Ok(Value::Float(v.into()))
    }
    fn serialize_f64(self, v: f64) -> Result<Value, Error> {
        Ok(Value::Float(v))
    }
    fn serialize_char(self, v: char) -> Result<Value, Error> {
        Ok(Value::Text(v.to_string()))
    }
    fn serialize_str(self, v: &str) -> Result<Value, Error> {
        Ok(Value::Text(v.to_owned()))
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<Value, Error> {
        Ok(Value::Bytes(v.to_vec()))
    }
    fn serialize_none(self) -> Result<Value, Error> {
        Ok(Value::Null)
    }
    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<Value, Error> {
        value.serialize(self)
    }
    fn serialize_unit(self) -> Result<Value, Error> {
        Ok(Value::Null)
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<Value, Error> {
        Ok(Value::Null)
    }
    fn serialize_unit_variant(self, _: &'static str, _: u32, variant: &'static str) -> Result<Value, Error> {
        Ok(Value::Text(variant.to_owned()))
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(self, _: &'static str, value: &T) -> Result<Value, Error> {
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Value, Error> {
        Ok(Value::Map(vec![(variant.to_owned(), value.serialize(self)?)]))
    }
    fn serialize_seq(self, len: Option<usize>) -> Result<Seq, Error> {
        Ok(Seq(Vec::with_capacity(len.unwrap_or(0))))
    }
    fn serialize_tuple(self, len: usize) -> Result<Seq, Error> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_struct(self, _: &'static str, len: usize) -> Result<Seq, Error> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_variant(self, _: &'static str, _: u32, variant: &'static str, len: usize) -> Result<Variant<Seq>, Error> {
        Ok(Variant {
            name: variant,
            inner: Seq(Vec::with_capacity(len)),
        })
    }
    fn serialize_map(self, len: Option<usize>) -> Result<Map, Error> {
        Ok(Map {
            entries: Vec::with_capacity(len.unwrap_or(0)),
            key: None,
        })
    }
    fn serialize_struct(self, _: &'static str, len: usize) -> Result<Map, Error> {
        self.serialize_map(Some(len))
    }
    fn serialize_struct_variant(self, _: &'static str, _: u32, variant: &'static str, len: usize) -> Result<Variant<Map>, Error> {
        Ok(Variant {
            name: variant,
            inner: self.serialize_map(Some(len))?,
        })
    }
}

pub(super) struct Seq(Vec<Value>);

impl Seq {
    fn push<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.0.push(value.serialize(ValueSerializer)?);
        Ok(())
    }
}

impl ser::SerializeSeq for Seq {
    type Ok = Value;
    type Error = Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.push(value)
    }
    fn end(self) -> Result<Value, Error> {
        Ok(Value::Array(self.0))
    }
}

impl ser::SerializeTuple for Seq {
    type Ok = Value;
    type Error = Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.push(value)
    }
    fn end(self) -> Result<Value, Error> {
        Ok(Value::Array(self.0))
    }
}

impl ser::SerializeTupleStruct for Seq {
    type Ok = Value;
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.push(value)
    }
    fn end(self) -> Result<Value, Error> {
        Ok(Value::Array(self.0))
    }
}

/// Map entries in serialization order; encoding sorts them and rejects
/// duplicates.
pub(super) struct Map {
    entries: Vec<(String, Value)>,
    key: Option<String>,
}

impl ser::SerializeMap for Map {
    type Ok = Value;
    type Error = Error;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), Error> {
        self.key = Some(key.serialize(KeySerializer)?);
        Ok(())
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        let key = self.key.take().ok_or_else(|| Error::Schema("map value without a key".into()))?;
        self.entries.push((key, value.serialize(ValueSerializer)?));
        Ok(())
    }
    fn end(self) -> Result<Value, Error> {
        Ok(Value::Map(self.entries))
    }
}

impl ser::SerializeStruct for Map {
    type Ok = Value;
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, key: &'static str, value: &T) -> Result<(), Error> {
        self.entries.push((key.to_owned(), value.serialize(ValueSerializer)?));
        Ok(())
    }
    fn end(self) -> Result<Value, Error> {
        Ok(Value::Map(self.entries))
    }
}

/// An externally tagged enum variant with content: a one-entry map.
pub(super) struct Variant<T> {
    name: &'static str,
    inner: T,
}

impl<T> Variant<T> {
    fn wrap(name: &'static str, value: Value) -> Value {
        Value::Map(vec![(name.to_owned(), value)])
    }
}

impl ser::SerializeTupleVariant for Variant<Seq> {
    type Ok = Value;
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.inner.push(value)
    }
    fn end(self) -> Result<Value, Error> {
        Ok(Self::wrap(self.name, Value::Array(self.inner.0)))
    }
}

impl ser::SerializeStructVariant for Variant<Map> {
    type Ok = Value;
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, key: &'static str, value: &T) -> Result<(), Error> {
        ser::SerializeStruct::serialize_field(&mut self.inner, key, value)
    }
    fn end(self) -> Result<Value, Error> {
        Ok(Self::wrap(self.name, Value::Map(self.inner.entries)))
    }
}

/// Map keys must be text.
struct KeySerializer;

impl ser::Serializer for KeySerializer {
    type Ok = String;
    type Error = Error;
    type SerializeSeq = ser::Impossible<String, Error>;
    type SerializeTuple = ser::Impossible<String, Error>;
    type SerializeTupleStruct = ser::Impossible<String, Error>;
    type SerializeTupleVariant = ser::Impossible<String, Error>;
    type SerializeMap = ser::Impossible<String, Error>;
    type SerializeStruct = ser::Impossible<String, Error>;
    type SerializeStructVariant = ser::Impossible<String, Error>;

    fn is_human_readable(&self) -> bool {
        false
    }

    fn serialize_str(self, v: &str) -> Result<String, Error> {
        Ok(v.to_owned())
    }
    fn serialize_char(self, v: char) -> Result<String, Error> {
        Ok(v.to_string())
    }
    fn serialize_unit_variant(self, _: &'static str, _: u32, variant: &'static str) -> Result<String, Error> {
        Ok(variant.to_owned())
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(self, _: &'static str, value: &T) -> Result<String, Error> {
        value.serialize(self)
    }

    fn serialize_bool(self, _: bool) -> Result<String, Error> {
        Err(Error::NonTextKey)
    }
    fn serialize_i8(self, _: i8) -> Result<String, Error> {
        Err(Error::NonTextKey)
    }
    fn serialize_i16(self, _: i16) -> Result<String, Error> {
        Err(Error::NonTextKey)
    }
    fn serialize_i32(self, _: i32) -> Result<String, Error> {
        Err(Error::NonTextKey)
    }
    fn serialize_i64(self, _: i64) -> Result<String, Error> {
        Err(Error::NonTextKey)
    }
    fn serialize_u8(self, _: u8) -> Result<String, Error> {
        Err(Error::NonTextKey)
    }
    fn serialize_u16(self, _: u16) -> Result<String, Error> {
        Err(Error::NonTextKey)
    }
    fn serialize_u32(self, _: u32) -> Result<String, Error> {
        Err(Error::NonTextKey)
    }
    fn serialize_u64(self, _: u64) -> Result<String, Error> {
        Err(Error::NonTextKey)
    }
    fn serialize_f32(self, _: f32) -> Result<String, Error> {
        Err(Error::NonTextKey)
    }
    fn serialize_f64(self, _: f64) -> Result<String, Error> {
        Err(Error::NonTextKey)
    }
    fn serialize_bytes(self, _: &[u8]) -> Result<String, Error> {
        Err(Error::NonTextKey)
    }
    fn serialize_none(self) -> Result<String, Error> {
        Err(Error::NonTextKey)
    }
    fn serialize_some<T: Serialize + ?Sized>(self, _: &T) -> Result<String, Error> {
        Err(Error::NonTextKey)
    }
    fn serialize_unit(self) -> Result<String, Error> {
        Err(Error::NonTextKey)
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<String, Error> {
        Err(Error::NonTextKey)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(self, _: &'static str, _: u32, _: &'static str, _: &T) -> Result<String, Error> {
        Err(Error::NonTextKey)
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq, Error> {
        Err(Error::NonTextKey)
    }
    fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple, Error> {
        Err(Error::NonTextKey)
    }
    fn serialize_tuple_struct(self, _: &'static str, _: usize) -> Result<Self::SerializeTupleStruct, Error> {
        Err(Error::NonTextKey)
    }
    fn serialize_tuple_variant(self, _: &'static str, _: u32, _: &'static str, _: usize) -> Result<Self::SerializeTupleVariant, Error> {
        Err(Error::NonTextKey)
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, Error> {
        Err(Error::NonTextKey)
    }
    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self::SerializeStruct, Error> {
        Err(Error::NonTextKey)
    }
    fn serialize_struct_variant(self, _: &'static str, _: u32, _: &'static str, _: usize) -> Result<Self::SerializeStructVariant, Error> {
        Err(Error::NonTextKey)
    }
}
