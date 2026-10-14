# Setup encoding

Status: codec and conformance corpus only (#110). No setup message uses it yet;
the v3 handshake (#111) moves bootstrap and CPU setup onto it, and the C ABI
keeps exposing typed structs rather than encoded bytes.

Jackstay setup messages are CBOR under the stack profile of flotilla
[ADR 0063](https://github.com/flotilla-org/flotilla/blob/main/docs/adr/0063-cbor-is-the-stack-binary-format.md),
tracked here as [#110](https://github.com/flotilla-org/jackstay/issues/110).
Every message is encoded and decoded through `jackstay::cbor`; no protocol code
calls a CBOR crate directly. Shared-memory rings and descriptors stay fixed
`repr(C)` layouts and are never encoded.

## Data model

A message body is one data item built from unsigned and negative integers
(`-2^64` to `2^64 - 1`), floats, byte strings, UTF-8 text strings, arrays, maps
with text keys, `true`, `false` and `null`. Tags, `undefined` and other simple
values are outside the profile. The allowed-tag list is empty: admitting a tag
is a deliberate change to the codec and the corpus, never a decoder option.

## Encoding

Encoding is deterministic (RFC 8949 §4.2), so every value has exactly one byte
form:

- integers, lengths and counts use the shortest head;
- lengths are always definite;
- map entries are sorted bytewise by their encoded key, which for text keys
  means shorter keys first, then bytewise; a duplicate key is an error;
- a float uses the shortest of binary16, binary32 and binary64 that holds it
  exactly;
- a float with an integral value in the integer range is encoded as that
  integer. `-0.0`, the infinities and integral floats beyond `2^64 - 1` or
  below `-2^64` stay floats. NaN cannot be encoded.

An `f64` field holding `1.0` therefore travels as the integer `1`. Decoding
into an `f64` or `f32` field accepts an integer only when the float type holds
it exactly, and an `f32` field accepts only floats binary32 holds exactly.

## Decoding

The reader is strict. It rejects, with a specific error, everything the
encoder would not produce: a non-shortest head or float, an integral float,
NaN, an indefinite length or stray break, a reserved head, any tag, an
unsupported simple value, a non-text key, a duplicate or out-of-order key,
invalid UTF-8, a truncated item and bytes after the item.

Typed decoding ignores unknown map keys, so a later version may add a field.
A key is never reused with a new meaning. A message is a map whose `type` text
field names its kind; a type the receiver does not know is rejected before any
field is read. A signature or hash, if one is ever added, covers the bytes as
received, since dropping unknown keys means a re-encoding can differ.

## Framing

```
magic[8] | length: u32 big-endian | body[length]
```

The magic is protocol-specific; #111 chooses the setup magic. The length is
big-endian like Jackstay's existing length prefixes and bootstrap fields. The
reader checks the magic, then checks the length against the message limit
before allocating the body. The body is exactly one map.

## Limits

| Limit | ADR default | Jackstay setup |
| --- | --- | --- |
| Message body | 1 MiB | 64 KiB |
| Nesting depth | 64 | 64 |
| Elements per array or map | 65,536 | 65,536 |
| Byte string | the message limit | 4 KiB |

Opaque descriptors, such as those of app-defined track kinds, travel as byte
strings, so the setup profile caps every byte string at 4 KiB
(`SETUP_MAX_DESCRIPTOR`). Each limit is inclusive. Depth counts arrays and maps,
the outermost being depth 1. The decoder compares every declared length or
count with its limit and with the bytes left before using it, and never
reserves capacity from a declared length: a short message claiming 65,536
elements fails without allocating for them.

## Rust API

`Limits` with `DEFAULT_LIMITS` and `SETUP_LIMITS`; `encode_value` and
`decode_value` over `Value`; serde through `to_vec`, `from_slice`, `to_value`
and `from_value`; `Message` for typed messages with a known `type` list,
through `encode_message` and `decode_message`; and `Framing` for the stream
format. `Bytes` marks a byte string field, since serde writes `Vec<u8>` as an
array of integers.

The codec is hand-written rather than built on `ciborium` or `minicbor`: the
profile is mostly what a reader refuses, neither crate's reader enforces it,
and `minicbor` is not MIT or Apache-2.0 licensed. The module documentation
records the comparison.

## Conformance corpus

[`crates/jackstay/tests/cbor_corpus`](../../crates/jackstay/tests/cbor_corpus/README.md)
holds the golden vectors that define the profile, as JSON data that any
language can run. `cargo test` runs them through the Rust codec.
