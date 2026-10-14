//! Runs the language-neutral CBOR profile corpus in tests/cbor_corpus/.
use std::{fs, path::PathBuf};

use jackstay::cbor::{self, DEFAULT_LIMITS, Error, Framing, Limits, Message, SETUP_LIMITS};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

fn load(name: &str) -> Vec<Json> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/cbor_corpus").join(name);
    let file: Json = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let vectors = file["vectors"].as_array().unwrap().clone();
    assert!(!vectors.is_empty(), "{name} has no vectors");
    vectors
}

fn name(vector: &Json) -> &str {
    vector["name"].as_str().unwrap()
}

/// A hex string, or a list of hex strings and `{"repeat": hex, "count": n}`.
fn bytes(spec: &Json) -> Vec<u8> {
    match spec {
        Json::String(hex) => unhex(hex),
        Json::Array(parts) => parts
            .iter()
            .flat_map(|part| match part {
                Json::String(hex) => unhex(hex),
                Json::Object(repeat) => unhex(repeat["repeat"].as_str().unwrap()).repeat(repeat["count"].as_u64().unwrap() as usize),
                _ => panic!("bad byte part {part}"),
            })
            .collect(),
        _ => panic!("bad byte spec {spec}"),
    }
}

fn unhex(hex: &str) -> Vec<u8> {
    assert!(hex.len() % 2 == 0, "odd hex {hex}");
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

fn limits(vector: &Json) -> Limits {
    match vector["limits"].as_str().unwrap() {
        "default" => DEFAULT_LIMITS,
        "setup" => SETUP_LIMITS,
        other => panic!("unknown limits {other}"),
    }
}

fn check(vector: &Json, result: Result<impl std::fmt::Debug, Error>) {
    let expect = vector["expect"].as_str().unwrap();
    match result {
        Ok(_) if expect == "accept" => {}
        Ok(value) => panic!("{}: accepted {value:?}, expected {expect}", name(vector)),
        Err(error) => assert_eq!(error.kind(), expect, "{}: {error}", name(vector)),
    }
}

#[test]
fn canonical_vectors_round_trip() {
    for vector in load("canonical.json") {
        let value = diag::parse(vector["diag"].as_str().unwrap());
        let hex = bytes(&vector["hex"]);
        let decoded = cbor::decode_value(&hex, &DEFAULT_LIMITS).unwrap_or_else(|e| panic!("{}: {e}", name(&vector)));
        assert_eq!(decoded, value, "{}: decode", name(&vector));
        assert_eq!(
            cbor::encode_value(&value, &DEFAULT_LIMITS).unwrap(),
            hex,
            "{}: encode",
            name(&vector)
        );
    }
}

#[test]
fn encode_vectors() {
    for vector in load("encode.json") {
        let result = cbor::encode_value(&diag::parse(vector["diag"].as_str().unwrap()), &DEFAULT_LIMITS);
        match vector.get("error") {
            Some(error) => assert_eq!(result.unwrap_err().kind(), error.as_str().unwrap(), "{}", name(&vector)),
            None => assert_eq!(result.unwrap(), bytes(&vector["hex"]), "{}", name(&vector)),
        }
    }
}

#[test]
fn decode_vectors() {
    for vector in load("decode.json") {
        let input = bytes(&vector["hex"]);
        let result = cbor::decode_value(&input, &limits(&vector));
        if let Ok(value) = &result {
            // Whatever is accepted is already in its one deterministic form.
            assert_eq!(cbor::encode_value(value, &limits(&vector)).unwrap(), input, "{}", name(&vector));
        }
        check(&vector, result.map(|_| ()));
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CorpusMessage {
    Hello {},
    Goodbye {},
}

impl Message for CorpusMessage {
    const TYPES: &'static [&'static str] = &["hello", "goodbye"];
}

#[test]
fn frame_vectors() {
    for vector in load("frames.json") {
        let framing = Framing::new(*b"JSTEST01", limits(&vector));
        check(&vector, framing.decode::<CorpusMessage>(&bytes(&vector["hex"])));
    }
}

/// The subset of CBOR diagnostic notation (RFC 8949 §8) the corpus uses.
mod diag {
    use jackstay::cbor::Value;

    pub fn parse(text: &str) -> Value {
        let mut parser = Parser { text, pos: 0 };
        let value = parser.value();
        parser.space();
        assert_eq!(parser.pos, text.len(), "trailing diagnostic text in {text}");
        value
    }

    struct Parser<'a> {
        text: &'a str,
        pos: usize,
    }

    impl<'a> Parser<'a> {
        fn rest(&self) -> &'a str {
            &self.text[self.pos..]
        }

        fn space(&mut self) {
            self.pos += self.rest().len() - self.rest().trim_start().len();
        }

        fn eat(&mut self, token: &str) -> bool {
            self.space();
            let found = self.rest().starts_with(token);
            if found {
                self.pos += token.len();
            }
            found
        }

        fn expect(&mut self, token: &str) {
            assert!(self.eat(token), "expected {token} at {:?}", self.rest());
        }

        fn value(&mut self) -> Value {
            self.space();
            for (word, value) in [
                ("true", Value::Bool(true)),
                ("false", Value::Bool(false)),
                ("null", Value::Null),
                ("NaN", Value::Float(f64::NAN)),
                ("Infinity", Value::Float(f64::INFINITY)),
                ("-Infinity", Value::Float(f64::NEG_INFINITY)),
            ] {
                if self.eat(word) {
                    return value;
                }
            }
            if self.eat("[") {
                let mut items = Vec::new();
                while !self.eat("]") {
                    if !items.is_empty() {
                        self.expect(",");
                    }
                    items.push(self.value());
                }
                return Value::Array(items);
            }
            if self.eat("{") {
                let mut entries = Vec::new();
                while !self.eat("}") {
                    if !entries.is_empty() {
                        self.expect(",");
                    }
                    self.space();
                    let key = self.text_string();
                    self.expect(":");
                    entries.push((key, self.value()));
                }
                return Value::Map(entries);
            }
            if self.eat("h'") {
                let end = self.rest().find('\'').unwrap();
                let hex = &self.rest()[..end];
                let bytes = (0..hex.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                    .collect();
                self.pos += end + 1;
                return Value::Bytes(bytes);
            }
            if self.rest().starts_with('"') {
                return Value::Text(self.text_string());
            }
            self.number()
        }

        /// A JSON string literal.
        fn text_string(&mut self) -> String {
            let rest = self.rest();
            let mut end = 1;
            let bytes = rest.as_bytes();
            while bytes[end] != b'"' {
                end += if bytes[end] == b'\\' { 2 } else { 1 };
            }
            self.pos += end + 1;
            serde_json::from_str(&rest[..=end]).unwrap()
        }

        /// An integer, or a float when it has a fraction or exponent.
        fn number(&mut self) -> Value {
            let len = self
                .rest()
                .find(|c: char| !(c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E')))
                .unwrap_or(self.rest().len());
            let literal = &self.rest()[..len];
            self.pos += len;
            if literal.contains(['.', 'e', 'E']) {
                Value::Float(literal.parse().unwrap_or_else(|_| panic!("bad float {literal}")))
            } else {
                Value::Integer(literal.parse().unwrap_or_else(|_| panic!("bad integer {literal}")))
            }
        }
    }
}
