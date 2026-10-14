//! The strict reader: one data item, in deterministic form, within limits.
use super::{Error, Limits, Value, float};

pub(super) fn decode(bytes: &[u8], limits: &Limits) -> Result<Value, Error> {
    if bytes.len() > limits.max_message {
        return Err(Error::MessageLimit {
            len: bytes.len() as u64,
            limit: limits.max_message,
        });
    }
    let mut reader = Reader { bytes, pos: 0, limits };
    let value = reader.item(0)?;
    if reader.pos != bytes.len() {
        return Err(Error::TrailingBytes);
    }
    Ok(value)
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    limits: &'a Limits,
}

impl<'a> Reader<'a> {
    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], Error> {
        if len > self.remaining() {
            return Err(Error::Truncated);
        }
        let taken = &self.bytes[self.pos..self.pos + len];
        self.pos += len;
        Ok(taken)
    }

    fn byte(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    fn peek(&self) -> Result<u8, Error> {
        self.bytes.get(self.pos).copied().ok_or(Error::Truncated)
    }

    /// The argument of a head with `info`, and whether it is in shortest form.
    /// `None` for an indefinite length.
    fn argument(&mut self, info: u8) -> Result<Option<(u64, bool)>, Error> {
        let (value, shortest) = match info {
            0..=23 => (u64::from(info), true),
            24 => {
                let v = u64::from(self.byte()?);
                (v, v >= 24)
            }
            25 => {
                let v = u64::from(u16::from_be_bytes(self.take(2)?.try_into().unwrap()));
                (v, v > u8::MAX.into())
            }
            26 => {
                let v = u64::from(u32::from_be_bytes(self.take(4)?.try_into().unwrap()));
                (v, v > u16::MAX.into())
            }
            27 => {
                let v = u64::from_be_bytes(self.take(8)?.try_into().unwrap());
                (v, v > u32::MAX.into())
            }
            28..=30 => return Err(Error::Reserved(info)),
            _ => return Ok(None),
        };
        Ok(Some((value, shortest)))
    }

    /// A definite, shortest length for a string or container head.
    fn length(&mut self, info: u8) -> Result<u64, Error> {
        match self.argument(info)? {
            None => Err(Error::IndefiniteLength),
            Some((_, false)) => Err(Error::NonShortestLength),
            Some((len, true)) => Ok(len),
        }
    }

    /// `depth` counts the containers enclosing this item.
    fn item(&mut self, depth: usize) -> Result<Value, Error> {
        let initial = self.byte()?;
        let major = initial >> 5;
        let info = initial & 0x1f;
        match major {
            0 | 1 => {
                let (n, shortest) = self.argument(info)?.ok_or(Error::Reserved(info))?;
                if !shortest {
                    return Err(Error::NonShortestInteger);
                }
                let n = i128::from(n);
                Ok(Value::Integer(if major == 0 { n } else { -1 - n }))
            }
            2 => {
                let len = self.length(info)?;
                if len > self.limits.max_byte_string as u64 {
                    return Err(Error::ByteStringLimit {
                        len,
                        limit: self.limits.max_byte_string,
                    });
                }
                Ok(Value::Bytes(self.take(within(len, self.remaining())?)?.to_vec()))
            }
            3 => self.text_after_head(info).map(|text| Value::Text(text.to_owned())),
            4 => {
                let count = self.container(info, depth, 1)?;
                let mut items = Vec::new();
                for _ in 0..count {
                    items.push(self.item(depth + 1)?);
                }
                Ok(Value::Array(items))
            }
            5 => {
                let count = self.container(info, depth, 2)?;
                let mut entries: Vec<(String, Value)> = Vec::new();
                let mut previous: &[u8] = &[];
                for _ in 0..count {
                    let start = self.pos;
                    if self.peek()? >> 5 != 3 {
                        return Err(Error::NonTextKey);
                    }
                    self.pos += 1;
                    let key = self.text_after_head(self.bytes[start] & 0x1f)?;
                    let encoded = &self.bytes[start..self.pos];
                    if !entries.is_empty() {
                        match encoded.cmp(previous) {
                            std::cmp::Ordering::Equal => return Err(Error::DuplicateKey(key.to_owned())),
                            std::cmp::Ordering::Less => return Err(Error::UnsortedKeys),
                            std::cmp::Ordering::Greater => {}
                        }
                    }
                    previous = encoded;
                    let value = self.item(depth + 1)?;
                    entries.push((key.to_owned(), value));
                }
                Ok(Value::Map(entries))
            }
            6 => {
                let (tag, _) = self.argument(info)?.ok_or(Error::Reserved(info))?;
                Err(Error::Tag(tag))
            }
            _ => self.simple(info),
        }
    }

    fn text_after_head(&mut self, info: u8) -> Result<&'a str, Error> {
        let len = self.length(info)?;
        let bytes = self.take(within(len, self.remaining())?)?;
        std::str::from_utf8(bytes).map_err(|_| Error::InvalidUtf8)
    }

    /// Check a container head against the limits and the bytes left, each
    /// element needing at least `min_bytes`, before anything is read or held.
    fn container(&mut self, info: u8, depth: usize, min_bytes: u64) -> Result<u64, Error> {
        let count = self.length(info)?;
        if depth + 1 > self.limits.max_depth {
            return Err(Error::DepthLimit {
                limit: self.limits.max_depth,
            });
        }
        if count > self.limits.max_elements as u64 {
            return Err(Error::ElementLimit {
                count,
                limit: self.limits.max_elements,
            });
        }
        if count.saturating_mul(min_bytes) > self.remaining() as u64 {
            return Err(Error::Truncated);
        }
        Ok(count)
    }

    fn simple(&mut self, info: u8) -> Result<Value, Error> {
        let value = match info {
            20 => Value::Bool(false),
            21 => Value::Bool(true),
            22 => Value::Null,
            25 => {
                let f = float::from_f16(u16::from_be_bytes(self.take(2)?.try_into().unwrap()));
                Value::Float(check_float(f, 16)?)
            }
            26 => {
                let f = f64::from(f32::from_be_bytes(self.take(4)?.try_into().unwrap()));
                Value::Float(check_float(f, 32)?)
            }
            27 => {
                let f = f64::from_be_bytes(self.take(8)?.try_into().unwrap());
                Value::Float(check_float(f, 64)?)
            }
            28..=30 => return Err(Error::Reserved(info)),
            31 => return Err(Error::UnexpectedBreak),
            _ => return Err(Error::UnsupportedSimple),
        };
        Ok(value)
    }
}

/// A declared length that fits in the remaining input, as a `usize`.
fn within(len: u64, remaining: usize) -> Result<usize, Error> {
    if len > remaining as u64 {
        return Err(Error::Truncated);
    }
    Ok(len as usize)
}

/// A float read at `width` bits is acceptable only in its preferred form.
fn check_float(f: f64, width: u32) -> Result<f64, Error> {
    if f.is_nan() {
        return Err(Error::Nan);
    }
    if float::integral(f).is_some() {
        return Err(Error::IntegralFloat);
    }
    let shorter = match width {
        32 => float::to_f16(f).is_some(),
        64 => float::fits_f32(f),
        _ => false,
    };
    if shorter {
        return Err(Error::NonShortestFloat);
    }
    Ok(f)
}
