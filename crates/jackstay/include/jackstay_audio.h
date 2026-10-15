#ifndef JACKSTAY_AUDIO_H
#define JACKSTAY_AUDIO_H
#include "capture_transfer.h"
#ifdef __cplusplus
extern "C" {
#endif

/* Playback audio tracks, ABI 0.17. Each track is one single-writer ring of
 * fixed-size sample blocks in shared memory. The producer never waits for
 * consumers; each consumer reads at its own latency through a private cursor
 * and may be lapped without affecting anyone else. Tracks are offered,
 * selected and announced through jackstay_handshake.h.
 *
 * Threading: one thread at a time uses a writer, a reader or a doorbell wait,
 * and nothing is destroyed while in use. ft_playback_writer_write,
 * _discontinuity and _reconfigure never wait or allocate: a write copies into
 * shared memory and rings a coalesced, non-blocking wake, so it is safe on a
 * real-time audio thread. Reads are copies too. Other calls may block or
 * allocate. ft_playback, ring handles and ft_playback_doorbell_take may be
 * used from any thread. Timestamps are host time in ns. */

/* Positioned layouts use SDL/WAVE channel order. */
#define FT_AUDIO_LAYOUT_MONO 1u
#define FT_AUDIO_LAYOUT_STEREO 2u
#define FT_AUDIO_LAYOUT_2_1 3u
#define FT_AUDIO_LAYOUT_QUAD 4u
#define FT_AUDIO_LAYOUT_5_1 5u
#define FT_AUDIO_LAYOUT_7_1 6u
#define FT_AUDIO_LAYOUT_DISCRETE 7u /* any channel count, without positions */

/* Little-endian interleaved samples. */
#define FT_AUDIO_SAMPLE_U8 1u
#define FT_AUDIO_SAMPLE_S16 2u
#define FT_AUDIO_SAMPLE_S32 3u
#define FT_AUDIO_SAMPLE_F32 4u

/* ft_audio_block flags. */
#define FT_AUDIO_BLOCK_DISCONTINUITY 1u   /* e.g. after a pause: not drift */
#define FT_AUDIO_BLOCK_RECONFIGURATION 2u /* the format changed at this block's first sample */
#define FT_AUDIO_BLOCK_GAP 4u             /* this reader was lapped just before this block */

#define FT_AUDIO_MIN_DEPTH_NS 250000000ull

typedef struct ft_audio_format {
  uint32_t rate; /* Hz */
  uint16_t channels;
  uint8_t layout; /* FT_AUDIO_LAYOUT_* */
  uint8_t sample; /* FT_AUDIO_SAMPLE_* */
} ft_audio_format;

/* A playback track's offer: its format and ring geometry. */
typedef struct ft_audio_descriptor {
  ft_audio_format format;
  uint32_t block_frames; /* most frames one block holds in this format */
  uint32_t blocks;       /* ring depth */
} ft_audio_descriptor;

/* One block copied out of a ring. samples point into the reader's buffer: the
 * next read on that reader overwrites them, and destruction frees them.
 * Ignore reserved. */
typedef struct ft_audio_block {
  uint64_t cursor;
  uint64_t sample_position;
  uint64_t host_time_ns; /* of the first sample */
  const uint8_t *samples;
  size_t samples_len;
  ft_audio_format format;
  uint32_t frames;
  uint32_t generation; /* advances at each reconfiguration */
  uint32_t flags;      /* FT_AUDIO_BLOCK_* */
  uint32_t reserved;
} ft_audio_block;

typedef struct ft_playback_writer ft_playback_writer;
typedef struct ft_playback ft_playback;
typedef struct ft_playback_ring ft_playback_ring;
typedef struct ft_playback_reader ft_playback_reader;
typedef struct ft_playback_doorbell ft_playback_doorbell;

/* Producer. Create a track's ring: block_frames is the producer's period and
 * depth_ns at least FT_AUDIO_MIN_DEPTH_NS, rounded up to whole blocks. The
 * writer writes; *out_playback is the host's handle for offering the track.
 * Outputs start NULL. Writing continues with no readers. */
ft_status ft_playback_writer_create(uint64_t track_id, const ft_audio_format *format, uint32_t block_frames,
                                    uint64_t depth_ns, ft_playback_writer **out_writer,
                                    ft_playback **out_playback);
/* The writer's current descriptor, for ft_track_offer.audio. */
ft_status ft_playback_writer_descriptor(const ft_playback_writer *writer, ft_audio_descriptor *out);
/* Publish one block of whole interleaved frames, at most block_frames, stamped
 * with the host time of its first sample, and ring the doorbells. Every
 * failure is INVALID_ARGUMENT: the only one is a bad length or pointer. */
ft_status ft_playback_writer_write(ft_playback_writer *writer, const void *samples, size_t len,
                                   uint64_t host_time_ns, uint64_t *out_cursor);
/* Flag the next block as a discontinuity. */
ft_status ft_playback_writer_discontinuity(ft_playback_writer *writer);
/* Switch format at the next block, which carries the reconfiguration flag and
 * a new generation. Block size in bytes stays fixed. Then send the new
 * descriptor with ft_track_notices_update. */
ft_status ft_playback_writer_reconfigure(ft_playback_writer *writer, const ft_audio_format *format);
void ft_playback_writer_destroy(ft_playback_writer **writer);
void ft_playback_destroy(ft_playback **playback);

/* Consumer. A ring is a selected track's read-only mapping. Readers start at
 * (write cursor - latency_ns), rounded up to whole blocks and capped below
 * the ring depth, once the ring has a block. *out starts NULL. */
ft_status ft_playback_reader_create(const ft_playback_ring *ring, uint64_t latency_ns, ft_playback_reader **out);
/* The next block in order: OK, or EMPTY in underrun. Call it when the output
 * device needs data. */
ft_status ft_playback_reader_read(ft_playback_reader *reader, ft_audio_block *out);
/* Blocks ready to read (the check that needs no doorbell), delivery gaps and
 * underruns so far. */
ft_status ft_playback_reader_counters(const ft_playback_reader *reader, uint64_t *out_available,
                                      uint64_t *out_gaps, uint64_t *out_underruns);
void ft_playback_reader_destroy(ft_playback_reader **reader);
void ft_playback_ring_destroy(ft_playback_ring **ring);

/* One doorbell per incarnation with playback. Hint bit i means the track at
 * selection position i (mod 64) published since the last take. The hint is
 * optional: readers may always check their counters. Take clears it. */
uint64_t ft_playback_doorbell_take(const ft_playback_doorbell *doorbell);
/* Sleep until the hint is non-zero, then take it: OK, TIMEOUT or CANCELLED.
 * timeout_ns=0 checks once, UINT64_MAX waits indefinitely. One waiter per
 * doorbell: a wait drains the shared wake, so a second waiter could consume
 * the first one's wake and leave it asleep. take needs no such care. */
ft_status ft_playback_doorbell_wait(ft_playback_doorbell *doorbell, const ft_acquisition_cancellation *cancel,
                                    uint64_t timeout_ns, uint64_t *out_hint);
void ft_playback_doorbell_destroy(ft_playback_doorbell **doorbell);

#ifdef __cplusplus
}
#endif
#endif
