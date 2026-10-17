#ifndef JACKSTAY_HANDSHAKE_H
#define JACKSTAY_HANDSHAKE_H
#include "jackstay_input.h"
#include "jackstay_affordances.h"
#include "jackstay_audio.h"
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
 * Admitted video and playback tracks share one doorbell (jackstay_audio.h),
 * and each playback track maps a read-only ring. Offers, selections,
 * admissions and notices are typed here; their wire encoding never crosses
 * the ABI. Messages are at most 64 KiB, app-kind descriptors at most 4 KiB.
 *
 * Calls block within a five-second bound from accept/connect to admission
 * (input admission adds its own five seconds): run them on a worker. POSIX
 * callers holding a connected socket use ft_local_connection_adopt first. */

/* Track kinds: these core kinds, or a reverse-domain app kind such as
 * "org.example.cue" whose descriptor is opaque. Skip kinds you do not know. */
#define FT_TRACK_KIND_VIDEO "video"
#define FT_TRACK_KIND_INPUT "input"
#define FT_TRACK_KIND_AFFORDANCES "affordances"
#define FT_TRACK_KIND_PLAYBACK "playback"

#define FT_TRACK_FROM_SOURCE 0u
#define FT_TRACK_TO_SOURCE 1u
#define FT_TRACK_BOTH 2u

#define FT_TRACK_ADMITTED 0u
#define FT_TRACK_REFUSED_UNKNOWN 1u     /* no offered track has this id */
#define FT_TRACK_REFUSED_UNAVAILABLE 2u /* withheld, e.g. input without authority */
#define FT_TRACK_REFUSED_NOT_WRITABLE 3u /* write requested on a from-source track */

/* Track ids are stable and never reused within a publication. Strings are
 * NUL-terminated UTF-8. Only app kinds have (opaque) descriptor bytes; a
 * playback track is described by audio, which other kinds leave zero. */
typedef struct ft_track_offer {
  uint64_t id;
  const char *kind;
  const char *label;
  const uint8_t *descriptor;
  size_t descriptor_len;
  uint32_t direction; /* FT_TRACK_FROM_SOURCE, _TO_SOURCE or _BOTH */
  uint32_t reserved;
  ft_audio_descriptor audio;
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

#define FT_TRACK_NOTICE_ADDED 1u
#define FT_TRACK_NOTICE_REMOVED 2u /* only track.id is set */
#define FT_TRACK_NOTICE_UPDATED 3u /* same id and selection, new label or descriptor */

typedef struct ft_track_notice {
  uint32_t type; /* FT_TRACK_NOTICE_* */
  uint32_t reserved;
  ft_track_offer track;
} ft_track_notice;

typedef struct ft_handshake_offer ft_handshake_offer;
typedef struct ft_track_notices ft_track_notices;
typedef struct ft_tracks ft_tracks;

/* Source host. clock_domain is the one FT_CLOCK_DOMAIN_* of every track's
 * timestamps. Offer at most one input and one affordances track, and up to
 * 64 playback tracks, each with its ft_playback in playback. A non-NULL
 * target authorizes this peer's input on it; NULL refuses input tracks. The
 * target stays live until return; its executor keeps pumping. out_admissions
 * holds track_count entries and receives *out_admission_count of them in
 * selection order. On success the same connection returns in *connection:
 * serve CPU setup on it with ft_cpu_producer_serve_local when a video track
 * was admitted, which rings the doorbell's video bit. After basic checks
 * the connection is consumed on failure. Returned servers own workers; they
 * are not proof of input admission. *out_notices, NULL to decline, sends
 * notices and serves late selections; it also holds the playback doorbell,
 * so retain it for the incarnation's lifetime. */
ft_status ft_handshake_accept_local(ft_local_connection **connection, const char *source_label,
                                    uint32_t clock_domain, const ft_track_offer *tracks, size_t track_count,
                                    ft_playback *const *playback, size_t playback_count,
                                    ft_input_target *authorized_input,
                                    ft_track_admission *out_admissions, size_t *out_admission_count,
                                    ft_input_server **out_input,
                                    ft_affordances_producer **out_affordances,
                                    ft_track_notices **out_notices);

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
 * abandoning the source. These optional outputs may be NULL to decline:
 * out_playback receives count entries, the read-only ring of each admitted
 * playback track at its selection position and NULL elsewhere;
 * *out_doorbell is set when video or playback was admitted (video since
 * ABI 0.19); *out_tracks receives
 * notices and makes late selections. */
ft_status ft_handshake_select(ft_handshake_offer **offer, const ft_track_selection *selections,
                              size_t count, uint32_t input_mode, ft_track_admission *out_admissions,
                              ft_local_connection **out_connection, ft_input_client **out_input,
                              ft_status *out_input_status, ft_affordances_host **out_affordances,
                              ft_playback_ring **out_playback, ft_playback_doorbell **out_doorbell,
                              ft_tracks **out_tracks);
/* Abandon an unselected offer, closing its connection. NULL is harmless. */
void ft_handshake_offer_destroy(ft_handshake_offer **offer);

/* Tracks while attached, on the incarnation's tracks channel. Calls on one
 * handle are serialized; they block up to timeout_ms (0 polls) or the
 * five-second setup bound once a message starts. Any failure ends the
 * channel: destroy the handle.
 *
 * Host: offer a playback (with its ft_playback) or app-kind track under a
 * new id; update its label or descriptor (a playback descriptor must still
 * fit its ring: reconfigure the writer first); withdraw it. Consumers keep
 * a removed track's ring until they destroy it. */
ft_status ft_track_notices_add(ft_track_notices *notices, const ft_track_offer *track,
                               const ft_playback *playback);
ft_status ft_track_notices_update(ft_track_notices *notices, const ft_track_offer *track);
ft_status ft_track_notices_remove(ft_track_notices *notices, uint64_t id);
/* Answer one late selection if it starts within timeout_ms: OK, or EMPTY if
 * none did. Fails once the consumer has gone. */
ft_status ft_track_notices_serve(ft_track_notices *notices, uint32_t timeout_ms);
void ft_track_notices_destroy(ft_track_notices **notices);

/* Consumer: the next notice (OK) or EMPTY if none arrives within timeout_ms.
 * Its strings are borrowed until the next call on this handle. */
ft_status ft_tracks_next(ft_tracks *tracks, uint32_t timeout_ms, ft_track_notice *out);
/* Select one offered playback or app-kind track late, without disturbing
 * other selections; the host must be serving. *out_position is the doorbell
 * hint bit (mod 64). An admitted playback track's ring is in *out_ring, and
 * brings *out_doorbell if the incarnation has none yet. Handle outputs start
 * NULL. Notices that arrive first stay queued for ft_tracks_next. */
ft_status ft_tracks_select(ft_tracks *tracks, const ft_track_selection *selection,
                           ft_track_admission *out_admission, uint32_t *out_position,
                           ft_playback_ring **out_ring, ft_playback_doorbell **out_doorbell);
void ft_tracks_destroy(ft_tracks **tracks);

#ifdef __cplusplus
}
#endif
#endif
