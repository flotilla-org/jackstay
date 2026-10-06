#ifndef JACKSTAY_INPUT_H
#define JACKSTAY_INPUT_H
#include "capture_transfer.h"
#ifdef __cplusplus
extern "C" {
#endif
#if defined(__unix__) || defined(__APPLE__) || defined(_WIN32)
/* Shared input, ABI 0.14. Host supplies authorized, connected Unix streams, or
 * (ABI 0.9, all platforms) Local Endpoint connections via the *_local calls.
 * Setup is blocking (bounded to five seconds on the client); run off GUI/input
 * threads. Established connections have independent network/heartbeat workers.
 * Handle destruction must not race calls. Other calls are thread safe except
 * that a work handle has one owner. No fork or copied handle ownership.
 */
typedef struct ft_input_target ft_input_target;
typedef struct ft_input_server ft_input_server;
typedef struct ft_input_client ft_input_client;
typedef struct ft_input_work ft_input_work;
#define FT_INPUT_MODE_PHYSICAL 1
#define FT_INPUT_MODE_SOURCE_TEXT 2
#define FT_INPUT_MODE_COOPERATIVE 4
#define FT_INPUT_CAP_PHYSICAL 1
#define FT_INPUT_CAP_LOGICAL 2
#define FT_INPUT_CAP_TEXT 4
#define FT_INPUT_CAP_POINTER 8
#define FT_INPUT_CAP_SCROLL 16
#define FT_INPUT_KEY 1
#define FT_INPUT_TEXT 2
#define FT_INPUT_MOTION 3
#define FT_INPUT_BUTTON 4
#define FT_INPUT_SCROLL 5
#define FT_INPUT_CLEANUP 6
#define FT_INPUT_DOWN 1
#define FT_INPUT_UP 2
#define FT_INPUT_REPEAT 3
#define FT_INPUT_PHYSICAL_KEY 1
#define FT_INPUT_LOGICAL_KEY 2
/* Scroll contract (ABI 0.14, input wire v2): positive x/y move content right/down.
 * Preserve delivered natural-scroll deltas; never invert because of metadata.
 * SDL converts positive-up y to positive-down, retaining x's sign.
 * Scroll events never coalesce, including zero-delta lifecycle events.
 * Executors own line height/page size; fractions remain unrounded.
 */
/* Closed scalar values, not bit masks. Zero means unknown, distinct from NONE.
 * Non-scroll events require all scroll metadata fields to be zero. */
#define FT_INPUT_SCROLL_PHASE_UNKNOWN 0
#define FT_INPUT_SCROLL_PHASE_NONE 1
#define FT_INPUT_SCROLL_PHASE_MAY_BEGIN 2
#define FT_INPUT_SCROLL_PHASE_BEGAN 3
#define FT_INPUT_SCROLL_PHASE_STATIONARY 4
#define FT_INPUT_SCROLL_PHASE_CHANGED 5
#define FT_INPUT_SCROLL_PHASE_ENDED 6
#define FT_INPUT_SCROLL_PHASE_CANCELLED 7
#define FT_INPUT_MOMENTUM_PHASE_UNKNOWN 0
#define FT_INPUT_MOMENTUM_PHASE_NONE 1
#define FT_INPUT_MOMENTUM_PHASE_BEGAN 2
#define FT_INPUT_MOMENTUM_PHASE_CHANGED 3
#define FT_INPUT_MOMENTUM_PHASE_ENDED 4
#define FT_INPUT_SCROLL_INVERSION_UNKNOWN 0
#define FT_INPUT_SCROLL_INVERSION_FALSE 1
#define FT_INPUT_SCROLL_INVERSION_TRUE 2
/* Precise/continuous devices (trackpads, Magic Mouse, high-resolution wheels
 * reporting pixel deltas): target logical units, as geometry/pointer positions,
 * never device pixels. */
#define FT_INPUT_SCROLL_PIXEL 1
/* Notched wheels: one unit per notch (Windows delta / 120; X11 buttons 4-7:
 * 4 = y -1 (up), 5 = y +1 (down), 6 = x -1 (left), 7 = x +1 (right);
 * macOS non-precise: line delta). For native positive-up sources (Windows
 * WM_MOUSEWHEEL delta / 120 and macOS positive-up line deltas), negate the
 * vertical delta to make it positive-down. X11 values above are already
 * normalized. Apply platform natural-scrolling inversion before sending;
 * do not reapply an inversion already included in the platform event. */
#define FT_INPUT_SCROLL_LINE 2
/* Explicit page-scroll gestures only; never synthesized from wheels. */
#define FT_INPUT_SCROLL_PAGE 3
#define FT_INPUT_SCOPE_ALL 1
#define FT_INPUT_SCOPE_POINTER 2
#define FT_INPUT_EXECUTED 0
#define FT_INPUT_REJECTED 1
#define FT_INPUT_UNSUPPORTED 2
#define FT_INPUT_PARTIAL 3
#define FT_INPUT_UNCERTAIN 4
#define FT_INPUT_COMPLETED 1
#define FT_INPUT_REFUSED 2
#define FT_INPUT_RESET 3
#define FT_INPUT_CLOSED 4
/* sequence contains the number of superseded motions, not a sequence ID. */
#define FT_INPUT_COALESCED 5
#define FT_INPUT_REASON_FOCUS 1
#define FT_INPUT_REASON_GEOMETRY 2
#define FT_INPUT_REASON_DISCONNECT 3
#define FT_INPUT_REASON_EXPIRED 4
#define FT_INPUT_REASON_OVERFLOW 5
#define FT_INPUT_REASON_EXECUTION 6
/* Modifier metadata, never an independent held-state owner. */
#define FT_INPUT_SHIFT 1
#define FT_INPUT_CONTROL 2
#define FT_INPUT_ALT 4
#define FT_INPUT_SUPER 8
#define FT_INPUT_ALT_GRAPH 16
#define FT_INPUT_META 32
#define FT_INPUT_CAPS_LOCK 64
#define FT_INPUT_NUM_LOCK 128

typedef struct { uint64_t revision; double width, height; } ft_input_geometry;
typedef struct {
  uint32_t modes, capabilities, max_events, max_bytes, max_text_bytes, idle_timeout_ms;
  uint32_t independent_contributions, interaction_cancel;
  ft_input_geometry geometry;
} ft_input_config;
/* key is a NUL-terminated UTF-8 DOM code (physical) or key meaning (logical).
 * Every executor receives DOM KeyboardEvent.code names for physical-mode keys
 * on the wire, owns platform translation and its tables (not the library/C ABI),
 * and completes unmappable codes as FT_INPUT_UNSUPPORTED without guessing
 * (client poll reports FT_INPUT_COMPLETED with result FT_INPUT_UNSUPPORTED;
 * see docs/design/input.md).
 * press is opaque and nonzero; repeat/up use the binding recorded by down.
 * Text is length-delimited UTF-8, copied by send; no SDL-sized text restriction.
 * x/y are target-local logical positions for motion/button and fractional deltas
 * for scroll. Scroll position is pointer_x/y, positive deltas right/down.
 * New positional events require the current revision; release does not.
 * STATIONARY requires zero x/y. Metadata applies to every scroll unit.
 * At admission/reset the target rejects orphan phased continuations as STALE
 * until a fresh MAY_BEGIN/BEGAN. Unknown and known-unphased wheels remain valid.
 * Cleanup cancels gesture/momentum at the executor's retained recipient, even
 * without pointer buttons, and settles before publishing RESET.
 * max_bytes must be at least 112; each payload-free event is charged 112 bytes.
 */
typedef struct {
  uint32_t kind, action, key_kind, modifiers;
  uint64_t press, geometry_revision;
  uint32_t button, scroll_unit;
  double x, y, pointer_x, pointer_y;
  char key[64];
  const uint8_t *text;
  size_t text_len;
  uint32_t scroll_phase, scroll_momentum_phase, scroll_inverted_from_device;
} ft_input_event;
typedef struct {
  uint64_t controller, epoch, sequence;
  uint32_t scope, reason, mode, reserved;
  ft_input_event event;
} ft_input_operation;
typedef struct {
  uint32_t kind;
  int32_t result;
  uint64_t sequence, epoch;
  uint32_t reason, clean;
  ft_input_geometry geometry;
} ft_input_status;

void ft_input_config_default(ft_input_config *out);
ft_status ft_input_target_create(const ft_input_config *config, ft_input_target **out);
/* After valid pointers/nonnegative fd/null output, consumes fd and sets -1 on
 * every outcome. Same ownership rule as CPU setup. No retained descriptor copies
 * or concurrent caller I/O. Library may retain private worker-owned copies. */
#if defined(__unix__) || defined(__APPLE__)
ft_status ft_input_target_serve(ft_input_target *target, int32_t *fd, ft_input_server **out);
#endif
/* Same as target_serve for a connection the host accepted and authorized;
 * *connection is consumed and set to NULL on every outcome after basic checks. */
ft_status ft_input_target_serve_local(ft_input_target *target, ft_local_connection **connection,
                                      ft_input_server **out);
/* Nonblocking: EMPTY or one owned work item. Only one item may be in flight.
 * describe borrows text from work until complete. Complete consumes work, even
 * when cleanup fails. Executor must settle dispatched work before completing.
 * Cleanup releases only this controller's contribution; it does not undo effects.
 */
ft_status ft_input_target_next(ft_input_target *target, ft_input_work **out);
ft_status ft_input_work_describe(const ft_input_work *work, ft_input_operation *out);
ft_status ft_input_work_complete(ft_input_work **work, uint32_t outcome);
ft_status ft_input_target_geometry(ft_input_target *target, const ft_input_geometry *geometry);
/* Explicit host assertion that failed cleanup was resolved, e.g. executor rebuilt. */
ft_status ft_input_target_resolve(ft_input_target *target);
/* DRAINING keeps the handle live until controller cleanup finishes; failed
 * cleanup gives RECOVERY_REQUIRED. Destroy server/client first, then pump work. */
ft_status ft_input_target_destroy(ft_input_target **target);
#if !defined(__cplusplus) && defined(__STDC_VERSION__) && __STDC_VERSION__ >= 201112L && UINTPTR_MAX == UINT64_MAX
_Static_assert(sizeof(ft_input_config) == 56, "input config ABI");
_Static_assert(sizeof(ft_input_event) == 168, "input event ABI");
_Static_assert(offsetof(ft_input_event, scroll_phase) == 152, "input scroll phase ABI");
_Static_assert(offsetof(ft_input_event, scroll_momentum_phase) == 156, "input momentum phase ABI");
_Static_assert(offsetof(ft_input_event, scroll_inverted_from_device) == 160, "input inversion ABI");
_Static_assert(offsetof(ft_input_event, text) == 136, "input text pointer ABI");
_Static_assert(sizeof(ft_input_operation) == 208, "input operation ABI");
_Static_assert(sizeof(ft_input_status) == 56, "input status ABI");
_Static_assert(offsetof(ft_input_status, sequence) == 8, "input coalesced count ABI");
_Static_assert(FT_INPUT_COALESCED == 5, "input coalesced status ABI");
#endif
/* OK means transport worker ended, not that executor cleanup succeeded. */
ft_status ft_input_server_poll(const ft_input_server *server);
void ft_input_server_destroy(ft_input_server **server);
#if defined(__unix__) || defined(__APPLE__)
ft_status ft_input_client_connect(int32_t *fd, uint32_t mode, ft_input_client **out);
#endif
/* Same as client_connect on a connection from ft_local_connect; *connection is
 * consumed and set to NULL on every outcome after basic checks. */
ft_status ft_input_client_connect_local(ft_local_connection **connection, uint32_t mode,
                                        ft_input_client **out);
ft_status ft_input_client_describe(const ft_input_client *client, ft_input_config *out, uint64_t *controller, uint64_t *epoch);
/* OK means copied into a bounded send queue, not received/executed. Sequence
 * identifies a later completion or rejection, unless superseded by a motion.
 * COALESCED reports a count of settled superseded operations; lost outcomes
 * must not be replayed.
 * Physical mode rejects source repeats; source-text/cooperative use source repeat.
 */
ft_status ft_input_client_send(ft_input_client *client, const ft_input_event *event, uint64_t *sequence);
ft_status ft_input_client_poll(ft_input_client *client, ft_input_status *out);
ft_status ft_input_client_reset(ft_input_client *client);
/* Close starts orderly cleanup, observed through poll. Destroy disconnects and
 * joins only the transport worker, never asserts executor cleanup completed. */
void ft_input_client_close(ft_input_client *client);
void ft_input_client_destroy(ft_input_client **client);
#endif
#ifdef __cplusplus
}
#endif
#endif
