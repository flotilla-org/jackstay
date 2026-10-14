//! Deterministic encoding (RFC 8949 §4.2) of a [`Value`].
use super::{Error, Limits, Value, float};

/// The CBOR integer range: `-2^64 ..= 2^64 - 1`.
pub(super) const INTEGER_MIN: i128 = -(1 << 64);
pub(super) const INTEGER_MAX: i128 = (1 << 64) - 1;

pub(super) fn encode(value: &Value, limits: &Limits) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    Encoder { out: &mut out, limits }.item(value, 0)?;
    if out.len() > limits.max_message {
        return Err(Error::MessageLimit {
            len: out.len() as u64,
            limit: limits.max_message,
        });
    }
    Ok(out)
}

struct Encoder<'a> {
    out: &'a mut Vec<u8>,
    limits: &'a Limits,
}

impl Encoder<'_> {
    /// `depth` counts the containers enclosing `value`.
    fn item(&mut self, value: &Value, depth: usize) -> Result<(), Error> {
        match value {
            Value::Null => self.out.push(0xf6),
            Value::Bool(false) => self.out.push(0xf4),
            Value::Bool(true) => self.out.push(0xf5),
            Value::Integer(n) => self.integer(*n)?,
            Value::Float(f) => self.float(*f)?,
            Value::Bytes(bytes) => {
                if bytes.len() > self.limits.max_byte_string {
                    return Err(Error::ByteStringLimit {
                        len: bytes.len() as u64,
                        limit: self.limits.max_byte_string,
                    });
                }
                head(self.out, 2, bytes.len() as u64);
                self.out.extend_from_slice(bytes);
            }
            Value::Text(text) => text_string(self.out, text),
            Value::Array(items) => {
                self.container(items.len(), depth)?;
                head(self.out, 4, items.len() as u64);
                for item in items {
                    self.item(item, depth + 1)?;
                }
            }
            Value::Map(entries) => {
                self.container(entries.len(), depth)?;
                let mut sorted: Vec<(Vec<u8>, &str, &Value)> = entries
                    .iter()
                    .map(|(key, value)| {
                        let mut encoded = Vec::with_capacity(key.len() + 9);
                        text_string(&mut encoded, key);
                        (encoded, key.as_str(), value)
                    })
                    .collect();
                sorted.sort_by(|a, b| a.0.cmp(&b.0));
                if let Some(pair) = sorted.windows(2).find(|pair| pair[0].0 == pair[1].0) {
                    return Err(Error::DuplicateKey(pair[0].1.to_owned()));
                }
                head(self.out, 5, entries.len() as u64);
                for (key, _, value) in sorted {
                    self.out.extend_from_slice(&key);
                    self.item(value, depth + 1)?;
                }
            }
        }
        Ok(())
    }

    fn container(&self, len: usize, depth: usize) -> Result<(), Error> {
        if depth + 1 > self.limits.max_depth {
            return Err(Error::DepthLimit {
                limit: self.limits.max_depth,
            });
        }
        if len > self.limits.max_elements {
            return Err(Error::ElementLimit {
                count: len as u64,
                limit: self.limits.max_elements,
            });
        }
        Ok(())
    }

    fn integer(&mut self, n: i128) -> Result<(), Error> {
        if !(INTEGER_MIN..=INTEGER_MAX).contains(&n) {
            return Err(Error::IntegerOutOfRange);
        }
        if n >= 0 {
            head(self.out, 0, n as u64);
        } else {
            head(self.out, 1, (-1 - n) as u64);
        }
        Ok(())
    }

    /// Numeric reduction first, then the shortest exact width.
    fn float(&mut self, f: f64) -> Result<(), Error> {
        if f.is_nan() {
            return Err(Error::Nan);
        }
        if let Some(n) = float::integral(f) {
            return self.integer(n);
        }
        if let Some(half) = float::to_f16(f) {
            self.out.push(0xf9);
            self.out.extend_from_slice(&half.to_be_bytes());
        } else if float::fits_f32(f) {
            self.out.push(0xfa);
            self.out.extend_from_slice(&(f as f32).to_be_bytes());
        } else {
            self.out.push(0xfb);
            self.out.extend_from_slice(&f.to_be_bytes());
        }
        Ok(())
    }
}

fn text_string(out: &mut Vec<u8>, text: &str) {
    head(out, 3, text.len() as u64);
    out.extend_from_slice(text.as_bytes());
}

/// The shortest head for `major` and `argument`.
fn head(out: &mut Vec<u8>, major: u8, argument: u64) {
    let major = major << 5;
    if argument < 24 {
        out.push(major | argument as u8);
    } else if argument <= u8::MAX.into() {
        out.extend_from_slice(&[major | 24, argument as u8]);
    } else if argument <= u16::MAX.into() {
        out.push(major | 25);
        out.extend_from_slice(&(argument as u16).to_be_bytes());
    } else if argument <= u32::MAX.into() {
        out.push(major | 26);
        out.extend_from_slice(&(argument as u32).to_be_bytes());
    } else {
        out.push(major | 27);
        out.extend_from_slice(&argument.to_be_bytes());
    }
}
