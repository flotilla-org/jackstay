#ifndef JACKSTAY_AFFORDANCES_H
#define JACKSTAY_AFFORDANCES_H
#include "jackstay_input.h"
#ifdef __cplusplus
extern "C" {
#endif
/* ABI 0.12. Every bool/present flag is 0 or 1. Input payloads are borrowed
 * only until return and copied. Event views are borrowed until event_destroy.
 * Domain tags: media=1 navigation=2 cursor=3 scroll=4 window=5 presentation=6.
 * Media status: unknown=0 playing=1 paused=2 stopped=3 buffering=4.
 * Artwork kind: null=0 URL=1 named icon=2. Cursor tags follow the table below.
 * Capabilities are bits (1u << (verb_tag-1)), scoped to the domain.
 * Media verbs: play=1 pause=2 stop=3 next=4 previous=5 absolute=6 relative=7.
 * Navigation verbs: back=1 forward=2 reload=3 stop=4 load=5.
 * Scroll verbs: step=1 position=2. axis: x=0 y=1; step: small=0 large=1;
 * direction: decrement=0 increment=1. number carries seconds/offset/position.
 * seek_absolute requires nonnegative position; seek_relative is signed;
 * scroll set_position is finite and producer-clamped, including negatives.
 * navigation.load URL is untrusted host input; policy belongs to the producer.
 * Event kind: snapshot=1 verb=2 closed=3. Closure clears cached state.
 * Poll: EMPTY if no event, OK with owned event. Send OK means enqueue only.
 * Methods require live, nonconcurrently destroyed handles. Output handles
 * start NULL. Destroy accepts pointer-to-handle and nulls it; close is idempotent.
 * Closing affordances never confirms input cleanup. No heartbeat/idle expiry.
 */
#define FT_AFF_DOMAIN_MEDIA 1u
#define FT_AFF_DOMAIN_NAVIGATION 2u
#define FT_AFF_DOMAIN_CURSOR 3u
#define FT_AFF_DOMAIN_SCROLL 4u
#define FT_AFF_DOMAIN_WINDOW 5u
#define FT_AFF_DOMAIN_PRESENTATION 6u
#define FT_AFF_MEDIA_PLAY 1u
#define FT_AFF_MEDIA_PAUSE 2u
#define FT_AFF_MEDIA_STOP 3u
#define FT_AFF_MEDIA_NEXT 4u
#define FT_AFF_MEDIA_PREVIOUS 5u
#define FT_AFF_MEDIA_SEEK_ABSOLUTE 6u
#define FT_AFF_MEDIA_SEEK_RELATIVE 7u
#define FT_AFF_NAVIGATION_BACK 1u
#define FT_AFF_NAVIGATION_FORWARD 2u
#define FT_AFF_NAVIGATION_RELOAD 3u
#define FT_AFF_NAVIGATION_STOP 4u
#define FT_AFF_NAVIGATION_LOAD 5u
#define FT_AFF_SCROLL_SCROLL_BY_STEP 1u
#define FT_AFF_SCROLL_SET_POSITION 2u
#define FT_AFF_CURSOR_AUTO 0u
#define FT_AFF_CURSOR_DEFAULT 1u
#define FT_AFF_CURSOR_NONE 2u
#define FT_AFF_CURSOR_CONTEXT_MENU 3u
#define FT_AFF_CURSOR_HELP 4u
#define FT_AFF_CURSOR_POINTER 5u
#define FT_AFF_CURSOR_PROGRESS 6u
#define FT_AFF_CURSOR_WAIT 7u
#define FT_AFF_CURSOR_CELL 8u
#define FT_AFF_CURSOR_CROSSHAIR 9u
#define FT_AFF_CURSOR_TEXT 10u
#define FT_AFF_CURSOR_VERTICAL_TEXT 11u
#define FT_AFF_CURSOR_ALIAS 12u
#define FT_AFF_CURSOR_COPY 13u
#define FT_AFF_CURSOR_MOVE 14u
#define FT_AFF_CURSOR_NO_DROP 15u
#define FT_AFF_CURSOR_NOT_ALLOWED 16u
#define FT_AFF_CURSOR_GRAB 17u
#define FT_AFF_CURSOR_GRABBING 18u
#define FT_AFF_CURSOR_E_RESIZE 19u
#define FT_AFF_CURSOR_N_RESIZE 20u
#define FT_AFF_CURSOR_NE_RESIZE 21u
#define FT_AFF_CURSOR_NW_RESIZE 22u
#define FT_AFF_CURSOR_S_RESIZE 23u
#define FT_AFF_CURSOR_SE_RESIZE 24u
#define FT_AFF_CURSOR_SW_RESIZE 25u
#define FT_AFF_CURSOR_W_RESIZE 26u
#define FT_AFF_CURSOR_EW_RESIZE 27u
#define FT_AFF_CURSOR_NS_RESIZE 28u
#define FT_AFF_CURSOR_NESW_RESIZE 29u
#define FT_AFF_CURSOR_NWSE_RESIZE 30u
#define FT_AFF_CURSOR_COL_RESIZE 31u
#define FT_AFF_CURSOR_ROW_RESIZE 32u
#define FT_AFF_CURSOR_ALL_SCROLL 33u
#define FT_AFF_CURSOR_ZOOM_IN 34u
#define FT_AFF_CURSOR_ZOOM_OUT 35u
typedef struct ft_aff_string {
    const uint8_t * data;
    size_t len;
} ft_aff_string;
typedef struct ft_aff_optional_string {
    uint32_t present;
    ft_aff_string value;
} ft_aff_optional_string;
typedef struct ft_aff_optional_number {
    uint32_t present;
    double value;
} ft_aff_optional_number;
typedef struct ft_aff_size {
    uint32_t present;
    double width;
    double height;
} ft_aff_size;
typedef struct ft_aff_artwork {
    uint32_t kind;
    ft_aff_string value;
} ft_aff_artwork;
typedef struct ft_aff_media {
    uint32_t status;
    ft_aff_optional_number position;
    double rate;
    ft_aff_optional_number duration;
    ft_aff_optional_string title;
    ft_aff_artwork artwork;
    uint32_t capabilities;
} ft_aff_media;
typedef struct ft_aff_navigation {
    ft_aff_optional_string url;
    ft_aff_optional_string title;
    uint32_t can_go_back;
    uint32_t can_go_forward;
    uint32_t loading;
    uint32_t capabilities;
} ft_aff_navigation;
typedef struct ft_aff_axis {
    uint32_t scrollable;
    double content_length;
    double viewport_length;
    double position;
} ft_aff_axis;
typedef struct ft_aff_scroll {
    ft_aff_axis x;
    ft_aff_axis y;
    uint32_t capabilities;
} ft_aff_scroll;
typedef struct ft_aff_window {
    ft_aff_optional_string title;
    ft_aff_size requested_size;
    uint32_t ready;
} ft_aff_window;
typedef struct ft_aff_presentation {
    uint32_t visible;
    ft_aff_size preferred_size;
    double scale;
    uint32_t focused;
} ft_aff_presentation;
typedef struct ft_aff_snapshot {
    uint32_t domain;
    uint32_t withdrawn;
    ft_aff_media media;
    ft_aff_navigation navigation;
    uint32_t cursor;
    ft_aff_scroll scroll;
    ft_aff_window window;
    ft_aff_presentation presentation;
} ft_aff_snapshot;
typedef struct ft_aff_verb {
    uint32_t domain;
    uint32_t verb;
    double number;
    ft_aff_string url;
    uint32_t axis;
    uint32_t step;
    uint32_t direction;
} ft_aff_verb;
typedef struct ft_aff_event_view {
    uint32_t kind;
    ft_aff_snapshot snapshot;
    ft_aff_verb verb;
} ft_aff_event_view;
typedef struct ft_affordances_producer ft_affordances_producer;
typedef struct ft_affordances_host ft_affordances_host;
typedef struct ft_affordances_event ft_affordances_event;
ft_status ft_affordances_producer_publish(ft_affordances_producer *, const ft_aff_snapshot *);
ft_status ft_affordances_producer_poll(ft_affordances_producer *, ft_affordances_event **);
void ft_affordances_producer_close(ft_affordances_producer *);
void ft_affordances_producer_destroy(ft_affordances_producer **);
ft_status ft_affordances_host_publish(ft_affordances_host *, const ft_aff_snapshot *);
ft_status ft_affordances_host_poll(ft_affordances_host *, ft_affordances_event **);
void ft_affordances_host_close(ft_affordances_host *);
void ft_affordances_host_destroy(ft_affordances_host **);
ft_status ft_affordances_host_send(ft_affordances_host *, const ft_aff_verb *);
ft_status ft_affordances_event_view(const ft_affordances_event *, ft_aff_event_view *);
void ft_affordances_event_destroy(ft_affordances_event **);
/* Explicit v2 Local Endpoint bootstrap; legacy entry points select v1.
 * affordances_request: NONE=0 OPTIONAL=1 REQUIRED=2. accept enable: 0/1.
 * Same consumption rules as jackstay_bootstrap.h. On success media remains
 * in *connection; independent handles transfer to caller. Optional refusal
 * returns NULL plus UNSUPPORTED; unrequested returns NULL plus EMPTY.
 */
ft_status ft_source_bootstrap_accept_v2_local(ft_local_connection **,
    ft_input_target *, uint32_t, ft_input_server **, ft_affordances_producer **);
/* POSIX descriptor variant: sole ownership, same output rules as the local
 * variant. After validation *fd becomes -1, then receives media on success.
 * The caller must verify the selected source peer before calling. */
#if defined(__unix__) || defined(__APPLE__)
ft_status ft_source_bootstrap_connect_v2(int32_t *fd,
    uint32_t input_request, uint32_t input_mode, uint32_t affordances_request,
    ft_input_client **, ft_status *input_status,
    ft_affordances_host **, ft_status *affordances_status);
#endif
ft_status ft_source_bootstrap_connect_v2_local(ft_local_connection **,
    uint32_t, uint32_t, uint32_t, ft_input_client **, ft_status *,
    ft_affordances_host **, ft_status *);
#ifdef __cplusplus
}
#endif
#endif
