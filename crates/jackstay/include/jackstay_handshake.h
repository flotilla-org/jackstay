#ifndef JACKSTAY_HANDSHAKE_H
#define JACKSTAY_HANDSHAKE_H
#include "jackstay_input.h"
#include "jackstay_affordances.h"
#ifdef __cplusplus
extern "C" {
#endif

/* Handshake v3 on one host-selected Local Endpoint connection, before CPU
 * setup on the same connection. The source offers its tracks, the consumer
 * selects some as optional or required (requesting to write tracks that flow
 * toward the source; input always requests it), and the host admits them.
 * Admitted input and affordances tracks each get their own channel; an
 * admitted video track is carried by this connection's CPU setup, which keeps
 * the consumer's kernel identity. A required refusal closes setup on both
 * sides (FT_STATUS_UNSUPPORTED); optional refusals are reported per track.
 * Offers, selections and admissions are typed here; their wire encoding never
 * crosses the ABI. Messages are at most 64 KiB, descriptors at most 4 KiB.
 *
 * Calls block within a five-second bound from accept/connect to admission
 * (input admission adds its own five seconds): run them on a worker. POSIX
 * callers holding a connected socket use ft_local_connection_adopt first. */

/* Track kinds: these core kinds, or a reverse-domain app kind such as
 * "org.example.cue" whose descriptor is opaque. Skip kinds you do not know. */
#define FT_TRACK_KIND_VIDEO "video"
#define FT_TRACK_KIND_INPUT "input"
#define FT_TRACK_KIND_AFFORDANCES "affordances"

#define FT_TRACK_FROM_SOURCE 0u
#define FT_TRACK_TO_SOURCE 1u
#define FT_TRACK_BOTH 2u

#define FT_TRACK_ADMITTED 0u
#define FT_TRACK_REFUSED_UNKNOWN 1u     /* no offered track has this id */
#define FT_TRACK_REFUSED_UNAVAILABLE 2u /* withheld, e.g. input without authority */
#define FT_TRACK_REFUSED_NOT_WRITABLE 3u /* write requested on a from-source track */

/* Track ids are stable and never reused within a publication. Strings are
 * NUL-terminated UTF-8. Core kinds have empty descriptors. */
typedef struct ft_track_offer {
  uint64_t id;
  const char *kind;
  const char *label;
  const uint8_t *descriptor;
  size_t descriptor_len;
  uint32_t direction; /* FT_TRACK_FROM_SOURCE, _TO_SOURCE or _BOTH */
  uint32_t reserved;
} ft_track_offer;

typedef struct ft_track_selection {
  uint64_t id;
  uint32_t required; /* 0 or 1 */
  uint32_t write;    /* 0 or 1 */
} ft_track_selection;

typedef struct ft_track_admission {
  uint64_t id;
  uint32_t result; /* FT_TRACK_ADMITTED or FT_TRACK_REFUSED_* */
  uint32_t reserved;
} ft_track_admission;

typedef struct ft_handshake_offer ft_handshake_offer;

/* Source host. clock_domain is the one FT_CLOCK_DOMAIN_* of every track's
 * timestamps. Offer at most one input and one affordances track. A non-NULL
 * target authorizes this peer's input on it; NULL refuses input tracks. The
 * target stays live until return; its executor keeps pumping. out_admissions
 * holds track_count entries and receives *out_admission_count of them in
 * selection order. On success the same connection returns in *connection:
 * serve CPU setup on it when a video track was admitted. After basic checks
 * the connection is consumed on failure. Returned servers own workers; they
 * are not proof of input admission. */
ft_status ft_handshake_accept_local(ft_local_connection **connection, const char *source_label,
                                    uint32_t clock_domain, const ft_track_offer *tracks, size_t track_count,
                                    ft_input_target *authorized_input,
                                    ft_track_admission *out_admissions, size_t *out_admission_count,
                                    ft_input_server **out_input,
                                    ft_affordances_producer **out_affordances);

/* Consumer: read the offer on a connection from ft_local_connect, whose
 * server was verified. After basic checks *connection moves into *out_offer
 * (and is consumed on failure). */
ft_status ft_handshake_connect_local(ft_local_connection **connection, ft_handshake_offer **out_offer);
/* Borrowed until the offer is selected or destroyed. */
ft_status ft_handshake_offer_describe(const ft_handshake_offer *offer, const char **out_source_label,
                                      uint32_t *out_clock_domain, const ft_track_offer **out_tracks,
                                      size_t *out_track_count);
/* input_mode is one FT_INPUT_MODE_* value, or 0 when no input track is
 * selected. out_admissions receives count entries in selection order. On
 * success the media connection is in *out_connection, ready for
 * ft_acquisition_cpu_connection_create_local. *out_input_status is EMPTY
 * without input, OK with an admitted client, or an optional input's clean
 * admission rejection (e.g. DRAINING when busy). After basic checks *offer is
 * consumed and nulled on every outcome; no handles escape a failure. Media and
 * channel lifetimes stay independent: close/poll/destroy input when
 * abandoning the source. */
ft_status ft_handshake_select(ft_handshake_offer **offer, const ft_track_selection *selections,
                              size_t count, uint32_t input_mode, ft_track_admission *out_admissions,
                              ft_local_connection **out_connection, ft_input_client **out_input,
                              ft_status *out_input_status, ft_affordances_host **out_affordances);
/* Abandon an unselected offer, closing its connection. NULL is harmless. */
void ft_handshake_offer_destroy(ft_handshake_offer **offer);

#ifdef __cplusplus
}
#endif
#endif
