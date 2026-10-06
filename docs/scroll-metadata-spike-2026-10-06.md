# macOS scroll metadata conversion spike

Run on 2026-10-06 on arm64 macOS 26.6 (25G72), Xcode 27.0 (27A266a).
The standalone Swift source and executable stayed under a temporary directory
outside the checkout. The full Swift source and observed output are in the
collapsible **Reproduce the macOS spike (source outside the repository)**
section of the [PR #93 body](https://github.com/flotilla-org/jackstay/pull/93),
not in the repository diff. Expand that section to reproduce the experiment.
The URL identifies GitHub's PR record, independently of the source branch or
intermediate commits; retain the reproduction in its description after merge.
[GitHub's merge reference](https://docs.github.com/en/pull-requests/reference/pull-request-merges)
describes squash as combining Git commits. As a repository check, the merged
[PR #91](https://github.com/flotilla-org/jackstay/pull/91) still returned its
description through authenticated `gh pr view` on 2026-10-06.
Nothing was posted to the system event stream; this test constructs CGEvents
and inspects NSEvents in process. It requires no desktop capture or event tap.

## Method and results

Create a fresh `CGEvent(scrollWheelEvent2Source: nil, units: .pixel,
wheelCount: 2, wheel1: 0, wheel2: 0, wheel3: 0)` for each sample. Set
`scrollWheelEventIsContinuous` to 1, set the fixed-point and point deltas, then
set `scrollWheelEventScrollPhase` and `scrollWheelEventMomentumPhase` with
CoreGraphics constants before converting with `NSEvent(cgEvent:)`.

The table contains observed raw values, not proposed wire encodings:

| Sample | CG scroll / momentum | NS phase / momentumPhase |
| --- | --- | --- |
| No phase | 0 / 0 | 0 / 0 |
| Physical began | 1 / 0 | 1 / 0 |
| Physical changed | 2 / 0 | 4 / 0 |
| Physical ended | 4 / 0 | 8 / 0 |
| Physical cancelled | 8 / 0 | 16 / 0 |
| Physical may-begin | 128 / 0 | 32 / 0 |
| Momentum begin | 0 / 1 | 0 / 1 |
| Momentum continue | 0 / 2 | 0 / 4 |
| Momentum end | 0 / 3 | 0 / 8 |
| Gesture-to-momentum handoff | 4 / 1 | 8 / 1 |
| End with `IsContinuous = 0` | 4 / 0 | 8 / 0 |
| Both ends with zero deltas | 4 / 3 | 8 / 8 |

All twelve assertions passed, including zero-delta `scrollingDeltaX/Y = 0` in
the final sample. Nonzero precise samples reported point deltas `-2, -4`;
line-mode samples reported fixed-point deltas `-2.5, -4.25`. This checks phase
conversion, not fractional-pixel fidelity in Luchs.

Two diagnostic cases demonstrate why executors must translate enums rather
than copy AppKit integers. Passing AppKit changed raw value 4 as the CG scroll
phase produced AppKit ended (8); passing AppKit ended raw value 8 as the CG
momentum phase produced no momentum phase (0). A combined CG scroll value
`began | changed` (3) also produced no AppKit phase (0). The public SDK has no
stationary `CGScrollPhase` constant. Preserve source stationary metadata in
Jackstay, but do not promise an exact public CGEvent stationary synthesis.

## Device-inversion probe

Inspection of the public SDK's `CGEventTypes.h` found no named device-inversion
field. The same constructor reported `isDirectionInvertedFromDevice = false`
for all phase samples. Setting each plausible public metadata candidate below
to 0 and 1 stored those values but still reported false:

| Candidate field | Observed inversion |
| --- | --- |
| `scrollWheelEventIsContinuous` | false for 0 and 1 |
| `scrollWheelEventScrollCount` | false for 0 and 1 |
| `scrollWheelEventMomentumOptionPhase` | false for 0 and 1 |

All six negative assertions passed. As a diagnostic only, the spike also
tested private field 135, named `kCGEventScrollGestureFlagBits` in
[WebKit's test SPI header](https://github.com/WebKit/WebKit/blob/main/Tools/TestRunnerShared/spi/CoreGraphicsTestSPI.h).
Setting individual bits 0 through 15 read back as 0 and kept inversion false
in this constructor. This is not a supported API or proof about every private
event representation; the design does not use it.

The conclusion is limited: no supported setter was found, and none of the
tested candidates conveyed inversion. Keep the optional transport bit, but
document Luchs's inability to expose true through this public construction
path. Delta signs must remain unchanged by the bit.

## Sources and limits

The installed public SDK headers define `CGScrollPhase`,
`CGMomentumScrollPhase`, and `NSEventPhase`. Apple's references describe the
[CG physical phase](https://developer.apple.com/documentation/coregraphics/cgscrollphase)
and [CG momentum phase](https://developer.apple.com/documentation/coregraphics/cgmomentumscrollphase)
types. [WebKit's own Swift test helper](https://github.com/WebKit/WebKit/blob/main/Tools/TestWebKitAPI/Helpers/cocoa/WebPage%2BExtras.swift)
uses the same public phase-setting route. Its
[macOS event factory](https://github.com/WebKit/WebKit/blob/main/Source/WebCore/platform/mac/PlatformEventFactoryMac.mm)
reads both native phases and the inversion property.

This experiment proves field conversion on this OS/SDK, not WebKit delivery,
rubber-banding, scroll snapping, swipe navigation, actual trackpad input,
cross-platform native mappings or recipient cleanup. The implementation must
test those separately. In particular,
[SDL2's wheel sender](https://github.com/libsdl-org/SDL/blob/SDL2/src/events/SDL_mouse.c)
drops a zero-delta event before its filter runs, so the current viewer filter
alone cannot forward terminal phases.
