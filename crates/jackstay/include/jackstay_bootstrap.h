#ifndef JACKSTAY_BOOTSTRAP_H
#define JACKSTAY_BOOTSTRAP_H
#include "jackstay_input.h"
#ifdef __cplusplus
extern "C" {
#endif

/* ABI 0.8: one already connected, host-authorized source endpoint.
 * Unix-only, as for the current CPU setup and input transports.
 * Bootstrap runs BEFORE CPU setup. It retains the original connection for
 * media so CPU admission still sees the consumer's actual kernel PID. Input
 * uses a separate transferred connection; it does not share media backpressure.
 */
#define FT_BOOTSTRAP_INPUT_NONE 0u
#define FT_BOOTSTRAP_INPUT_OPTIONAL 1u
#define FT_BOOTSTRAP_INPUT_REQUIRED 2u

/* Both calls block on setup: run on a worker, never an input/render thread.
 * Bootstrap has a five-second absolute deadline; connect may additionally spend
 * up to five seconds in input admission. No frames or input events flow here.
 *
 * All writable arguments are valid/disjoint, *fd owns a live connected Unix
 * SOCK_STREAM, and handle outputs start NULL. Basic invalid arguments leave fd
 * alone. After validation fd is consumed/set to -1 on EVERY failure. On success
 * the same fd is returned in *fd, now ready for the existing CPU setup API.
 * Do not retain duplicates or concurrently use the stream. No fork/forward/replay
 * of CPU grants is allowed; the caller remains the original connected process.
 * The stream is blocking on success; socket timeouts are unchanged.
 */

/* Host selects the media producer and input target for this SAME source before
 * calling. A non-NULL target authorizes this peer to request that target's input;
 * NULL permits observation only. Never infer input authority from media access.
 * Target must stay live until return; its executor continues pumping afterward.
 * On success pass fd to ft_cpu_producer_serve and independently retain the input
 * server, if any. A returned server is a worker owner, not proof of admission.
 * On failure/teardown continue pumping target cleanup, as for target_serve.
 */
ft_status ft_source_bootstrap_accept(int32_t *fd, ft_input_target *authorized_input,
                                    ft_input_server **out_input_server);

/* input_request is one of the constants above. input_mode is zero for NONE,
 * otherwise one FT_INPUT_MODE_* value. On success pass fd to
 * ft_acquisition_cpu_connection_create and attach through the existing API.
 * out_input_status: EMPTY for NONE, OK with an admitted input client, or the
 * actual clean admission rejection for OPTIONAL (e.g. UNSUPPORTED, DRAINING).
 * REQUIRED rejection returns that error and consumes fd; no handles escape.
 * Protocol/transport failure fails the whole bootstrap, even for OPTIONAL.
 *
 * Media and input lifetimes remain independent. If later media attach fails,
 * explicitly close/poll/destroy input when abandoning the source association.
 * Input destroy alone does not confirm executor cleanup.
 */
ft_status ft_source_bootstrap_connect(int32_t *fd, uint32_t input_request,
                                     uint32_t input_mode, ft_input_client **out_input,
                                     ft_status *out_input_status);
#ifdef __cplusplus
}
#endif
#endif
