# CBOR profile conformance corpus

Golden vectors for the stack CBOR profile of flotilla
[ADR 0063](https://github.com/flotilla-org/flotilla/blob/main/docs/adr/0063-cbor-is-the-stack-binary-format.md),
as applied to Jackstay setup messages ([design note](../../../../docs/design/setup-encoding.md),
[#110](https://github.com/flotilla-org/jackstay/issues/110)). Every encoder and
decoder in the stack should pass them. `tests/cbor_corpus.rs` runs them against
`jackstay::cbor`.

The vectors are plain JSON so other languages can reuse them. Each file is an
object with a `description` and a `vectors` list. Edit the vectors by hand and
check every new byte sequence against an independent encoder.

## Files

| File | Vectors | Check |
| --- | --- | --- |
| `canonical.json` | 71 | Decoding `hex` gives `diag`, and encoding `diag` gives `hex`. |
| `encode.json` | 16 | Encoding `diag` gives `hex`, or fails with `error`. |
| `decode.json` | 122 | Decoding `hex` under `limits` is accepted, or fails with `expect`. An accepted input re-encodes to the same bytes. |
| `frames.json` | 23 | Decoding one frame from `hex` under `limits` is accepted, or fails with `expect`. |

## Fields

- `hex`: lowercase hex, or a list of parts. A part is a hex string or
  `{"repeat": "<hex>", "count": n}`, which keeps limit vectors small.
- `diag`: a value in the subset of CBOR diagnostic notation (RFC 8949 §8) the
  corpus needs: decimal integers; floats written with a fraction or exponent,
  plus `Infinity`, `-Infinity` and `NaN`; JSON string literals; `h'..'` byte
  strings; `[..]` arrays; `{..}` maps with text keys; `true`, `false`, `null`.
  A literal with a fraction or exponent is a float even when integral, so
  `1.0` in `encode.json` tests numeric reduction. Floats compare by bit
  pattern, so `-0.0` differs from `0.0`. In `canonical.json`, map entries are
  written in canonical order; `encode.json` writes some out of order on
  purpose.
- `limits`: `default` (ADR defaults: 1,048,576 bytes per message, depth 64,
  65,536 elements per array or map, byte strings up to the message limit) or
  `setup` (65,536 bytes per message, depth 64, 65,536 elements, 4,096 bytes per
  byte string, which is where opaque descriptors travel).
- `expect` and `error`: `accept`, or the name of the first violation a reader
  finds, reading in order. A decoder in another language may report errors
  differently but must reject exactly these inputs.

| Error | Meaning |
| --- | --- |
| `truncated` | Input ends inside a data item or frame, or a declared length or count exceeds the bytes left. |
| `trailing-bytes` | Bytes follow the data item or frame. |
| `reserved` | Additional information 28 to 30, or 31 on an integer, tag or other head that has no indefinite form. |
| `indefinite-length` | An indefinite-length string, array or map. |
| `unexpected-break` | A break code outside an indefinite-length item. |
| `non-shortest-integer` | An integer whose head is longer than needed. |
| `non-shortest-length` | A length or count whose head is longer than needed. |
| `non-shortest-float` | A float that a shorter width holds exactly. |
| `integral-float` | A float with an integral value in `-2^64 ..= 2^64 - 1`, other than `-0.0`. |
| `nan` | Any NaN, whatever its payload or width. |
| `tag` | Any tag: the allowed-tag list is empty. |
| `unsupported-simple` | `undefined` or any simple value other than `false`, `true` and `null`. |
| `non-text-key` | A map key that is not a text string. |
| `duplicate-key` | Two equal keys in one map. |
| `unsorted-keys` | Keys not in ascending bytewise order of their encodings. |
| `invalid-utf8` | A text string that is not UTF-8. |
| `integer-out-of-range` | An integer outside `-2^64 ..= 2^64 - 1` (encoding only). |
| `depth-limit` | Arrays and maps nested deeper than the limit; the outermost is depth 1. |
| `element-limit` | An array or map declaring more elements than the limit. |
| `message-limit` | A message body, or a frame's declared length, over the limit. |
| `byte-string-limit` | A byte string declaring more bytes than the limit. |
| `not-a-map` | A frame body that is not a map. |
| `missing-type` | A frame body without a `type` key. |
| `type-not-text` | A `type` that is not a text string. |
| `unknown-type` | A `type` outside the protocol's list. |
| `bad-magic` | A frame magic other than the protocol's. |

Limits are checked against declared sizes before anything is read or allocated
for them. That is why a byte string declaring 2^64 - 1 bytes fails with
`byte-string-limit`, and an array declaring 65,536 elements in a one-byte
remainder fails with `truncated`, rather than either allocating.

## Frames

`frames.json` uses the test magic `JSTEST01` (hex `4a53544553543031`), a
big-endian u32 body length and a body map whose `type` must be `hello` or
`goodbye`. Other fields are ignored.

## Open questions

- **JSON rendering of infinities** is not decided. The proposal is
  `{"f64": "Infinity"}` and `{"f64": "-Infinity"}`, matching the ADR's integer
  escapes; it needs the owner's confirmation. Neither this corpus nor the codec
  renders JSON.
- **JSON integer cut-off**, carried over with it: ±(2^53 - 1) would render as a
  bare JSON number and ±2^53 as `{"u64": ..}` or `{"i64": ..}`. The canonical
  vectors include both boundaries so a future JSON rendering can pin against
  them.
