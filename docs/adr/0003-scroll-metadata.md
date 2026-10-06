---
status: proposed
---
# Scroll metadata travels with ordered deltas and shares pointer cleanup

[Issue #90](https://github.com/flotilla-org/jackstay/issues/90) requires a design
before implementation. Extend each scroll event with independent optional
physical phase, momentum phase and device-inversion information, in one public
ABI release. The [input contract](../design/input.md#planned-scroll-metadata)
defines the field shapes, C encodings, mappings and acceptance cases.
Keep this ADR proposed while the implementation is pending; the combined
implementation changes its status to accepted when it lands.

Unknown differs from a known unphased event and from a known false inversion
bit. Keep native phase numbers out of the shared contract. The
[macOS conversion spike](../scroll-metadata-spike-2026-10-06.md) verifies public
phase synthesis but finds no supported route for the inversion bit; carry the
bit without promising every executor can expose it. Signed deltas always
retain the controller's content direction, as decided in #88.

Scroll events do not coalesce. Geometry changes cancel the destination's
scroll interaction through pointer cleanup, even without held buttons. Settle
in-flight work before cancellation, retain its native recipient, and wait for
cleanup completion before accepting a fresh gesture. Reject stale deltas and
discard the old source gesture's tail; do not transplant it onto new geometry.
Focus loss and assignment-ending cleanup also close gesture and momentum.

## Alternatives considered

- **Native bit masks:** couple clients to AppKit and invite passing its raw
  values to CoreGraphics. The spike shows that even those two APIs disagree.
  Closed enums and explicit unknown values make translation visible.
- **Coalesce changes between phase boundaries:** would require rules for unit,
  recipient, geometry, both phase domains and inversion changes. Current
  Jackstay only coalesces motion; preserving every bounded scroll event keeps
  that contract and its visible overflow behaviour.
- **Keep a gesture alive across resize:** an old coordinate can name a different
  recipient under the new geometry, and the end event itself can be stale.
  Pointer cleanup already supplies the necessary ordering and acknowledgement.
- **Guess phases for portable wheels or use private macOS fields:** idle gaps
  cannot distinguish momentum or cancellation, and no public inversion setter
  survived the spike. Unknown metadata and an explicit executor limitation
  avoid promising synthetic device semantics that the source cannot supply.

## Consequences

The implementation plans ABI 0.14 and input wire version 2. If another change
consumes 0.14 before implementation, use the next unused minor for all three
fields together, updating this ADR and the input contract in the same change.
Rust, C, the SDL viewer and Luchs change together, along with exact-version consumers. This ADR
does not bump any version or add code. Implementation issues are filed after
the design PR merges; the direction-only work in #88 has its own scope.

Capturing zero-delta terminal events requires native macOS observation before
SDL's wheel filter discards them. End-to-end WebKit behaviour remains a separate
implementation acceptance task. Cancellation closes controller-owned state;
it cannot undo already committed scrolling or navigation.
