/* jackstay frame ring — canonical shared-memory layout (layout_version 3)
 *
 * This header is the wire-format contract for the jackstay video track
 * control page. The Rust implementation in src/control_page.rs mirrors these
 * structs and static-asserts every offset against the values documented here.
 * Producers and consumers in any language interoperate by agreeing on this
 * file alone; the function-call API lives in capture_transfer.h.
 *
 * Layout of the control page (one shared-memory mapping):
 *
 *   [jackstay_ring_header   256 B]
 *   [jackstay_frame_slot    128 B] x slot_capacity      (power of two)
 *   [jackstay_stream_config 128 B] x config_capacity    (power of two, small)
 *   [jackstay_consumer_slot 128 B] x consumer_capacity
 *
 * Every record is exactly one 128-byte cacheline (Apple silicon line size;
 * two lines on 64-byte machines, which is harmless) so no two records ever
 * false-share. All integers are little-endian, native-width loads/stores.
 *
 * Concurrency model (seqlock broadcast ring, single producer, many readers):
 *
 *   producer publish:
 *     slot.publication_sequence = 0          relaxed   (invalidate)
 *     release fence                                    (zero lands before data)
 *     <plain stores of the descriptor body>
 *     slot.publication_sequence = cursor     release   (publish slot)
 *     header.producer_cursor    = cursor     release   (publish discovery)
 *
 *   reader (for a wanted cursor C, slot index = (C-1) & (slot_capacity-1)):
 *     s1 = slot.publication_sequence         acquire
 *     <plain loads of the descriptor body; for cpu-shm payloads also copy
 *      the pixels now>
 *     acquire fence                                    (copies resolve first)
 *     s2 = slot.publication_sequence         relaxed
 *     valid iff s1 == C && s2 == C
 *
 * Both fences are required on weakly-ordered machines (ARM); they compile to
 * nothing on x86. The publication sequence doubles as invalidation flag
 * (zero), version counter, and frame identity: a published slot holds the
 * value of producer_cursor at the moment it was published, so lapping is
 * detected by the equality check alone. Cursors are monotonic 64-bit counts
 * starting at 1; slot indices are derived by masking, never recycled.
 *
 * Stream configs use the same discipline with config_generation as the
 * seqlock word (zero while mid-write). Configs change only on reconfigure
 * (resize, format change); readers cache the decoded config and re-read only
 * when a frame names an unfamiliar generation.
 */

#ifndef JACKSTAY_RING_H
#define JACKSTAY_RING_H

#include <stddef.h> /* offsetof, used by the _Static_asserts below */
#include <stdint.h>

/* "JSFRING1" read as a little-endian uint64_t. */
#define JACKSTAY_RING_MAGIC 0x31474E495246534AULL

#define JACKSTAY_RING_LAYOUT_VERSION 3

/* jackstay_frame_slot.payload_kind */
#define JACKSTAY_PAYLOAD_CPU_SHM             0u
#define JACKSTAY_PAYLOAD_IOSURFACE           1u
#define JACKSTAY_PAYLOAD_DMABUF              2u
#define JACKSTAY_PAYLOAD_D3D_SHARED_RESOURCE 3u

/* Header: line 0 is written once at creation and read-only thereafter;
 * line 1 holds the only hot cross-process words. */
typedef struct jackstay_ring_header {
    /* --- line 0: immutable geometry --- */
    uint64_t magic;             /*   0: JACKSTAY_RING_MAGIC                  */
    uint32_t layout_version;    /*   8: JACKSTAY_RING_LAYOUT_VERSION         */
    uint32_t header_len;        /*  12: sizeof(jackstay_ring_header)         */
    uint32_t slot_len;          /*  16: sizeof(jackstay_frame_slot)          */
    uint32_t slot_capacity;     /*  20: power of two                         */
    uint32_t config_len;        /*  24: sizeof(jackstay_stream_config)       */
    uint32_t config_capacity;   /*  28: power of two                         */
    uint32_t consumer_len;      /*  32: sizeof(jackstay_consumer_slot)       */
    uint32_t consumer_capacity; /*  36                                       */
    uint32_t slots_offset;      /*  40: from start of mapping                */
    uint32_t configs_offset;    /*  44                                       */
    uint32_t consumers_offset;  /*  48                                       */
    uint8_t  reserved0[76];     /*  52                                       */
    /* --- line 1: hot --- */
    uint64_t producer_cursor;   /* 128: ATOMIC. Count of published frames;
                                 *      0 = empty. Everything else about ring
                                 *      occupancy derives from it:
                                 *        len          = min(cursor, slot_capacity)
                                 *        latest index = (cursor-1) & (slot_capacity-1)
                                 *        oldest live  = cursor - len + 1        */
    uint64_t config_cursor;     /* 136: ATOMIC. Latest config generation;
                                 *      0 = none published yet.                 */
    uint8_t  reserved1[112];    /* 144                                       */
} jackstay_ring_header;         /* 256 */

/* One published frame. The payload is NOT here: cpu-shm payloads live in a
 * pool segment attached via the setup channel (pool_id names it, slot_id and
 * payload_offset address into it); native payloads ARE the pool slot (an
 * IOSurface / dmabuf / shared resource registered via the setup channel),
 * and payload_offset/len are zero. */
typedef struct jackstay_frame_slot {
    uint64_t publication_sequence;   /*   0: ATOMIC seqlock word, see above  */
    uint64_t sequence;               /*   8: producer frame count; unlike the
                                      *      cursor it advances on dropped
                                      *      frames, so gaps are visible      */
    uint64_t timestamp_ns;           /*  16: in the config's clock_domain     */
    uint64_t pool_id;                /*  24: unique forever, never reused —
                                      *      stale pools need no generation   */
    uint64_t payload_offset;         /*  32: bytes into the pool mapping      */
    uint64_t payload_len;            /*  40: meaningful payload bytes         */
    uint64_t fence_value;            /*  48: timeline value to wait for on
                                      *      the config's fence before
                                      *      sampling a native payload        */
    uint64_t damage_base_sequence;   /*  56: frame this damage is relative to */
    uint64_t producer_drop_count;    /*  64: lifetime cumulative drops        */
    uint32_t slot_id;                /*  72: slot index within the pool       */
    uint32_t config_generation;      /*  76: names a jackstay_stream_config   */
    uint32_t payload_kind;           /*  80: JACKSTAY_PAYLOAD_*               */
    uint32_t damage_kind;            /*  84                                   */
    uint32_t dropped_before_publish; /*  88: gap immediately before this one  */
    uint32_t flags;                  /*  92: reserved, zero                   */
    uint8_t  reserved[32];           /*  96                                   */
} jackstay_frame_slot;               /* 128 */

/* Per-stream values that change only on reconfigure. A resize is not a
 * special frame: it is a new generation (and, when dimensions grow, a new
 * pool) that subsequent frames reference. Generations are monotonic from 1;
 * the ring holds the last config_capacity of them at index
 * generation & (config_capacity-1), so readers may lag a reconfigure by up
 * to config_capacity-1 generations before a config is overwritten under
 * them (detected by the seqlock check). */
typedef struct jackstay_stream_config {
    uint64_t config_generation; /*   0: ATOMIC seqlock word; 0 = mid-write   */
    uint32_t width;             /*   8 */
    uint32_t height;            /*  12 */
    uint32_t stride;            /*  16: bytes per row (cpu-shm payloads)     */
    uint32_t pixel_format;      /*  20 */
    uint32_t color_space;       /*  24 */
    uint32_t clock_domain;      /*  28 */
    uint32_t sync_kind;         /*  32 */
    uint32_t reserved0;         /*  36 */
    uint64_t modifier;          /*  40: dmabuf format modifier               */
    uint64_t fence_id;          /*  48: the stream's timeline fence, as
                                 *      registered via the setup channel     */
    uint8_t  reserved1[72];     /*  56 */
} jackstay_stream_config;       /* 128 */

/* One line per consumer: consumers never share a cacheline, preserving
 * per-consumer independence. The producer allocates and frees slots
 * (consumer_id == 0 means free); a registered consumer stores only into its
 * own slot, and every field is written with release stores readable
 * individually — the slot is observational, never a synchronization edge. */
typedef struct jackstay_consumer_slot {
    uint64_t consumer_id;            /*   0: ATOMIC; 0 = slot free           */
    uint64_t release_cursor;         /*   8: frames at or below this cursor
                                      *      are no longer being sampled     */
    uint64_t last_acquired_cursor;   /*  16 */
    uint64_t last_acquired_sequence; /*  24 */
    uint64_t acquired_count;         /*  32 */
    uint64_t skipped_count;          /*  40 */
    uint8_t  reserved[80];           /*  48 */
} jackstay_consumer_slot;            /* 128 */

#if defined(__STDC_VERSION__) && __STDC_VERSION__ >= 201112L
_Static_assert(sizeof(jackstay_ring_header) == 256, "header is two cachelines");
_Static_assert(offsetof(jackstay_ring_header, producer_cursor) == 128, "hot words on their own line");
_Static_assert(sizeof(jackstay_frame_slot) == 128, "frame slot is one cacheline");
_Static_assert(offsetof(jackstay_frame_slot, publication_sequence) == 0, "seqlock word is field 0");
_Static_assert(offsetof(jackstay_frame_slot, slot_id) == 72, "frame slot packing");
_Static_assert(sizeof(jackstay_stream_config) == 128, "stream config is one cacheline");
_Static_assert(offsetof(jackstay_stream_config, modifier) == 40, "stream config packing");
_Static_assert(sizeof(jackstay_consumer_slot) == 128, "consumer slot is one cacheline");
#endif

#endif /* JACKSTAY_RING_H */
