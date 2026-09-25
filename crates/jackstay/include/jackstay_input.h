#ifndef JACKSTAY_INPUT_H
#define JACKSTAY_INPUT_H
#include "capture_transfer.h"
#ifdef __cplusplus
extern "C" {
#endif
#if defined(__unix__) || defined(__APPLE__) || defined(_WIN32)
/* Shared input, ABI 0.7. Host supplies authorized, connected Unix streams, or
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
#define FT_INPUT_SCROLL_PIXEL 1
#define FT_INPUT_SCROLL_LINE 2
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
 * press is opaque and nonzero; repeat/up use the binding recorded by down.
 * Text is length-delimited UTF-8, copied by send; no SDL-sized text restriction.
 * x/y are target-local logical positions for motion/button and fractional deltas
 * for scroll. Scroll position is pointer_x/y, positive deltas right/down.
 * New positional events require the current revision; release does not.
 */
typedef struct {
  uint32_t kind, action, key_kind, modifiers;
  uint64_t press, geometry_revision;
  uint32_t button, scroll_unit;
  double x, y, pointer_x, pointer_y;
  char key[64];
  const uint8_t *text;
  size_t text_len;
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
_Static_assert(sizeof(ft_input_event) == 152, "input event ABI");
_Static_assert(offsetof(ft_input_event, text) == 136, "input text pointer ABI");
_Static_assert(sizeof(ft_input_operation) == 192, "input operation ABI");
_Static_assert(sizeof(ft_input_status) == 56, "input status ABI");
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
 * identifies a later completion or rejection; lost outcomes must not be replayed.
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
