/* The playback track contract through the C ABI: the scenarios of
 * tests/playback.rs, driven by tests/playback_ffi.rs. Each scenario gets one
 * connected Local Endpoint pair and returns 0, or the failing line. */
#include "jackstay_handshake.h"
#include <string.h>
#if defined(_WIN32)
#include <windows.h>
typedef HANDLE thread;
#define THREAD_FN DWORD WINAPI
static int spawn(thread *t, LPTHREAD_START_ROUTINE fn, void *arg) { return (*t = CreateThread(NULL, 0, fn, arg, 0, NULL)) != NULL; }
static void join(thread t) { WaitForSingleObject(t, INFINITE); CloseHandle(t); }
#else
#include <pthread.h>
typedef pthread_t thread;
#define THREAD_FN void *
static int spawn(thread *t, void *(*fn)(void *), void *arg) { return pthread_create(t, NULL, fn, arg) == 0; }
static void join(thread t) { pthread_join(t, NULL); }
#endif

#define CHECK(x) do { if (!(x)) return __LINE__; } while (0)
#define OK(x) CHECK((x) == FT_STATUS_OK)
#define MS 1000000ull
#define FRAMES 480u
#define BLOCK (FRAMES * 4u)

static const ft_audio_format STEREO = {48000, 2, FT_AUDIO_LAYOUT_STEREO, FT_AUDIO_SAMPLE_S16};
static const ft_audio_format MONO = {44100, 1, FT_AUDIO_LAYOUT_MONO, FT_AUDIO_SAMPLE_F32};

typedef struct track {
  ft_playback_writer *writer;
  ft_playback *playback;
} track;

static ft_status create(uint64_t id, track *t) {
  t->writer = NULL, t->playback = NULL;
  return ft_playback_writer_create(id, &STEREO, FRAMES, 250 * MS, &t->writer, &t->playback);
}

/* Block n: every byte is n, stamped n * 10 ms. */
static ft_status write_block(track *t, uint8_t n) {
  uint8_t samples[BLOCK];
  memset(samples, n, sizeof samples);
  return ft_playback_writer_write(t->writer, samples, sizeof samples, n * 10 * MS, NULL);
}

static ft_track_offer offer(uint64_t id, const track *t, const char *label) {
  ft_track_offer o = {id, FT_TRACK_KIND_PLAYBACK, label, NULL, 0, FT_TRACK_FROM_SOURCE, 0, {{0}, 0, 0}};
  ft_playback_writer_descriptor(t->writer, &o.audio);
  return o;
}

static ft_track_selection selected(uint64_t id) { return (ft_track_selection){id, 1, 0}; }

typedef struct host {
  ft_local_connection *connection;
  ft_track_offer tracks[2];
  ft_playback *playback[2];
  size_t count;
  ft_track_notices *notices;
  ft_status status;
} host;

typedef struct viewer {
  ft_local_connection *media;
  ft_track_admission admissions[2];
  ft_playback_ring *rings[2];
  ft_playback_doorbell *doorbell;
  ft_tracks *tracks;
} viewer;

static THREAD_FN accept_job(void *arg) {
  host *h = arg;
  ft_track_admission admissions[2];
  size_t count = 0;
  ft_input_server *input = NULL;
  ft_affordances_producer *affordances = NULL;
  h->status = ft_handshake_accept_local(&h->connection, "C source", FT_CLOCK_DOMAIN_HOST_TIME, h->tracks, h->count,
                                        h->playback, h->count, NULL, admissions, &count, &input, &affordances,
                                        &h->notices);
  return 0;
}

/* Offer the host's tracks, check the typed offer and select. */
static int attach(host *h, ft_local_connection *peer, const ft_track_selection *selection, size_t count, viewer *v) {
  thread worker;
  CHECK(spawn(&worker, accept_job, h));
  ft_handshake_offer *handle = NULL;
  ft_status status = ft_handshake_connect_local(&peer, &handle);
  const char *source = NULL;
  uint32_t clock = 0;
  const ft_track_offer *tracks = NULL;
  size_t offered = 0;
  if (status == FT_STATUS_OK) status = ft_handshake_offer_describe(handle, &source, &clock, &tracks, &offered);
  for (size_t i = 0; status == FT_STATUS_OK && i < offered; i++)
    if (offered != h->count || tracks[i].id != h->tracks[i].id || strcmp(tracks[i].kind, FT_TRACK_KIND_PLAYBACK) ||
        tracks[i].descriptor_len != 0 || memcmp(&tracks[i].audio, &h->tracks[i].audio, sizeof tracks[i].audio))
      status = FT_STATUS_ERROR;
  ft_input_client *input = NULL;
  ft_status input_status;
  ft_affordances_host *affordances = NULL;
  memset(v, 0, sizeof *v);
  if (status == FT_STATUS_OK)
    status = ft_handshake_select(&handle, selection, count, 0, v->admissions, &v->media, &input, &input_status,
                                 &affordances, v->rings, &v->doorbell, &v->tracks);
  ft_handshake_offer_destroy(&handle);
  join(worker);
  OK(status);
  OK(h->status);
  return 0;
}

/* Writer 1, offered and selected alone. */
static int attach_one(ft_local_connection *a, ft_local_connection *b, track *t, host *h, viewer *v) {
  OK(create(1, t));
  *h = (host){a, {offer(1, t, "default")}, {t->playback}, 1, NULL, 0};
  ft_track_selection one = selected(1);
  return attach(h, b, &one, 1, v);
}

static void detach(host *h, viewer *v) {
  for (int i = 0; i < 2; i++) ft_playback_ring_destroy(&v->rings[i]);
  ft_playback_doorbell_destroy(&v->doorbell);
  ft_tracks_destroy(&v->tracks);
  ft_local_connection_destroy(&v->media);
  ft_track_notices_destroy(&h->notices);
  ft_local_connection_destroy(&h->connection);
}

static void discard(track *t) {
  ft_playback_writer_destroy(&t->writer);
  ft_playback_destroy(&t->playback);
}

/* One host serve, for a late selection made on the caller's thread. */
static THREAD_FN serve_job(void *arg) {
  host *h = arg;
  h->status = ft_track_notices_serve(h->notices, 5000);
  return 0;
}

static ft_status select_late(host *h, viewer *v, ft_track_selection selection, ft_track_admission *admission,
                             uint32_t *position, ft_playback_ring **ring, ft_playback_doorbell **doorbell) {
  thread worker;
  if (!spawn(&worker, serve_job, h)) return FT_STATUS_ERROR;
  ft_status status = ft_tracks_select(v->tracks, &selection, admission, position, ring, doorbell);
  join(worker);
  return status != FT_STATUS_OK ? status : h->status;
}

static int read_block(ft_playback_reader *reader, ft_audio_block *block) {
  return ft_playback_reader_read(reader, block) == FT_STATUS_OK;
}

static int offer_carries_the_descriptor_and_settings_floor_the_depth(ft_local_connection *a, ft_local_connection *b) {
  track t;
  OK(create(7, &t));
  ft_audio_descriptor descriptor;
  OK(ft_playback_writer_descriptor(t.writer, &descriptor));
  /* 250 ms at 48 kHz in 10 ms blocks. */
  CHECK(!memcmp(&descriptor.format, &STEREO, sizeof STEREO) && descriptor.block_frames == FRAMES &&
        descriptor.blocks == 25);
  host h = {a, {offer(7, &t, "default")}, {t.playback}, 1, NULL, 0};
  /* Playback descriptors are typed: encoded bytes are refused, unconsumed. */
  h.tracks[0].descriptor = (const uint8_t *)"x", h.tracks[0].descriptor_len = 1;
  ft_track_admission admission;
  size_t count = 0;
  ft_input_server *input = NULL;
  ft_affordances_producer *affordances = NULL;
  CHECK(ft_handshake_accept_local(&h.connection, "C source", FT_CLOCK_DOMAIN_HOST_TIME, h.tracks, 1, h.playback, 1,
                                  NULL, &admission, &count, &input, &affordances, NULL) == FT_STATUS_INVALID_ARGUMENT);
  CHECK(h.connection == a);
  h.tracks[0].descriptor = NULL, h.tracks[0].descriptor_len = 0;
  /* Playback flows only from the source. */
  viewer v;
  ft_track_selection writing = {7, 0, 1};
  CHECK(!attach(&h, b, &writing, 1, &v));
  CHECK(v.admissions[0].result == FT_TRACK_REFUSED_NOT_WRITABLE && !v.rings[0] && !v.doorbell);
  detach(&h, &v);
  track shallow = {NULL, NULL};
  CHECK(ft_playback_writer_create(1, &STEREO, FRAMES, 249 * MS, &shallow.writer, &shallow.playback) ==
        FT_STATUS_INVALID_ARGUMENT);
  discard(&t);
  return 0;
}

static int writes_continue_without_readers_and_never_block(ft_local_connection *a, ft_local_connection *b) {
  track t;
  OK(create(1, &t));
  for (int n = 0; n < 1000; n++) OK(write_block(&t, (uint8_t)n));
  host h = {a, {offer(1, &t, "default")}, {t.playback}, 1, NULL, 0};
  viewer v;
  ft_track_selection one = selected(1);
  CHECK(!attach(&h, b, &one, 1, &v));
  /* A consumer that never drains its doorbell does not stall the writer. */
  for (int n = 0; n < 10000; n++) OK(write_block(&t, (uint8_t)n));
  ft_playback_reader *reader = NULL;
  OK(ft_playback_reader_create(v.rings[0], 50 * MS, &reader));
  uint64_t available, gaps, underruns;
  OK(ft_playback_reader_counters(reader, &available, &gaps, &underruns));
  CHECK(available == 5);
  ft_playback_reader_destroy(&reader);
  detach(&h, &v);
  discard(&t);
  return 0;
}

static int each_consumer_starts_at_cursor_minus_its_latency_target(ft_local_connection *a, ft_local_connection *b) {
  track ahead, empty;
  OK(create(1, &ahead));
  OK(create(2, &empty));
  for (uint8_t n = 1; n <= 10; n++) OK(write_block(&ahead, n));
  host h = {a, {offer(1, &ahead, "default"), offer(2, &empty, "cue")}, {ahead.playback, empty.playback}, 2, NULL, 0};
  viewer v;
  ft_track_selection both[2] = {selected(1), selected(2)};
  CHECK(!attach(&h, b, both, 2, &v));
  ft_playback_reader *close_by = NULL, *behind = NULL, *first = NULL;
  OK(ft_playback_reader_create(v.rings[0], 20 * MS, &close_by));
  OK(ft_playback_reader_create(v.rings[0], 45 * MS, &behind));
  ft_audio_block block;
  CHECK(read_block(close_by, &block) && block.cursor == 9 && block.sample_position == 8 * FRAMES &&
        block.samples[0] == 9 && block.samples_len == BLOCK && block.flags == 0);
  /* 45 ms rounds up to five blocks. */
  CHECK(read_block(behind, &block) && block.cursor == 6);
  /* A reader joining an empty ring starts with its first block. */
  OK(ft_playback_reader_create(v.rings[1], 40 * MS, &first));
  CHECK(ft_playback_reader_read(first, &block) == FT_STATUS_EMPTY);
  OK(write_block(&empty, 1));
  CHECK(read_block(first, &block) && block.cursor == 1);
  ft_playback_reader_destroy(&close_by);
  ft_playback_reader_destroy(&behind);
  ft_playback_reader_destroy(&first);
  detach(&h, &v);
  discard(&ahead);
  discard(&empty);
  return 0;
}

static int underruns_are_counted_locally(ft_local_connection *a, ft_local_connection *b) {
  track t;
  host h;
  viewer v;
  CHECK(!attach_one(a, b, &t, &h, &v));
  ft_playback_reader *reader = NULL;
  OK(ft_playback_reader_create(v.rings[0], 0, &reader));
  ft_audio_block block;
  CHECK(ft_playback_reader_read(reader, &block) == FT_STATUS_EMPTY);
  OK(write_block(&t, 1));
  /* Zero latency waits for the next block after the write cursor. */
  CHECK(ft_playback_reader_read(reader, &block) == FT_STATUS_EMPTY);
  OK(write_block(&t, 2));
  CHECK(read_block(reader, &block) && block.samples[0] == 2);
  CHECK(ft_playback_reader_read(reader, &block) == FT_STATUS_EMPTY);
  uint64_t available, gaps, underruns;
  OK(ft_playback_reader_counters(reader, &available, &gaps, &underruns));
  CHECK(underruns == 3 && gaps == 0);
  ft_playback_reader_destroy(&reader);
  detach(&h, &v);
  discard(&t);
  return 0;
}

static int a_lapped_reader_resynchronizes_with_a_gap(ft_local_connection *a, ft_local_connection *b) {
  track t;
  host h;
  viewer v;
  CHECK(!attach_one(a, b, &t, &h, &v));
  ft_playback_reader *slow = NULL, *fast = NULL;
  OK(ft_playback_reader_create(v.rings[0], 30 * MS, &slow));
  OK(ft_playback_reader_create(v.rings[0], 30 * MS, &fast));
  ft_audio_block block;
  OK(write_block(&t, 1));
  CHECK(read_block(slow, &block) && block.cursor == 1);
  CHECK(read_block(fast, &block) && block.cursor == 1);
  for (uint8_t n = 2; n <= 100; n++) {
    OK(write_block(&t, n));
    CHECK(read_block(fast, &block) && block.cursor == n);
  }
  /* The ring holds 25 blocks: cursor 2 is long gone. */
  CHECK(read_block(slow, &block) && block.flags == FT_AUDIO_BLOCK_GAP && block.cursor == 98 && block.samples[0] == 98);
  CHECK(read_block(slow, &block) && block.flags == 0);
  uint64_t available, gaps, underruns;
  OK(ft_playback_reader_counters(slow, &available, &gaps, &underruns));
  CHECK(gaps == 1);
  OK(ft_playback_reader_counters(fast, &available, &gaps, &underruns));
  CHECK(gaps == 0);
  ft_playback_reader_destroy(&slow);
  ft_playback_reader_destroy(&fast);
  detach(&h, &v);
  discard(&t);
  return 0;
}

static int discontinuity_and_reconfiguration_mark_exact_samples(ft_local_connection *a, ft_local_connection *b) {
  track t;
  host h;
  viewer v;
  CHECK(!attach_one(a, b, &t, &h, &v));
  ft_playback_reader *reader = NULL;
  OK(ft_playback_reader_create(v.rings[0], 250 * MS, &reader));
  uint8_t samples[2 * BLOCK];
  OK(write_block(&t, 1));
  OK(ft_playback_writer_discontinuity(t.writer));
  memset(samples, 2, sizeof samples);
  OK(ft_playback_writer_write(t.writer, samples, 100 * 4, 5000 * MS, NULL));
  OK(ft_playback_writer_reconfigure(t.writer, &MONO));
  memset(samples, 3, sizeof samples);
  OK(ft_playback_writer_write(t.writer, samples, 64 * 4, 6000 * MS, NULL));
  OK(write_block(&t, 4));
  ft_audio_block block;
  CHECK(read_block(reader, &block) && block.flags == 0);
  CHECK(read_block(reader, &block) && block.flags == FT_AUDIO_BLOCK_DISCONTINUITY && block.sample_position == 480 &&
        block.host_time_ns == 5000 * MS && block.frames == 100);
  CHECK(read_block(reader, &block) && block.flags == FT_AUDIO_BLOCK_RECONFIGURATION && block.sample_position == 580 &&
        block.frames == 64 && !memcmp(&block.format, &MONO, sizeof MONO) && block.generation == 2 &&
        block.samples_len == 64 * 4 && !memcmp(block.samples, samples, 64 * 4));
  /* Same bytes per block: 480 S16 stereo frames are 480 F32 mono frames. */
  CHECK(read_block(reader, &block) && block.flags == 0 && block.generation == 2 && block.sample_position == 644 &&
        block.frames == 480);
  /* The new format must fit the block size fixed at creation. */
  ft_audio_format wide = {48000, 2, FT_AUDIO_LAYOUT_STEREO, FT_AUDIO_SAMPLE_F32};
  OK(ft_playback_writer_reconfigure(t.writer, &wide));
  CHECK(ft_playback_writer_write(t.writer, samples, sizeof samples, 0, NULL) == FT_STATUS_INVALID_ARGUMENT);
  ft_playback_reader_destroy(&reader);
  detach(&h, &v);
  discard(&t);
  return 0;
}

static THREAD_FN write_job(void *arg) {
  write_block(arg, 1);
  return 0;
}

static int the_doorbell_hints_changed_tracks(ft_local_connection *a, ft_local_connection *b) {
  track ta, tb;
  OK(create(1, &ta));
  OK(create(2, &tb));
  host h = {a, {offer(1, &ta, "default"), offer(2, &tb, "cue")}, {ta.playback, tb.playback}, 2, NULL, 0};
  viewer v;
  /* Bits follow selection positions: b is position 0, a is position 1. */
  ft_track_selection order[2] = {selected(2), selected(1)};
  CHECK(!attach(&h, b, order, 2, &v));
  ft_acquisition_cancellation *cancel = NULL;
  OK(ft_acquisition_cancellation_create(&cancel));
  uint64_t hint = 0;
  CHECK(ft_playback_doorbell_wait(v.doorbell, cancel, 10 * MS, &hint) == FT_STATUS_TIMEOUT);
  thread worker;
  CHECK(spawn(&worker, write_job, &ta));
  OK(ft_playback_doorbell_wait(v.doorbell, cancel, 5000 * MS, &hint));
  join(worker);
  CHECK(hint == 2);
  OK(write_block(&tb, 1));
  OK(write_block(&ta, 2));
  OK(write_block(&tb, 2));
  OK(ft_playback_doorbell_wait(v.doorbell, cancel, UINT64_MAX, &hint));
  CHECK(hint == 3 && ft_playback_doorbell_take(v.doorbell) == 0);
  OK(ft_acquisition_cancellation_cancel(cancel));
  CHECK(ft_playback_doorbell_wait(v.doorbell, cancel, UINT64_MAX, &hint) == FT_STATUS_CANCELLED);
  ft_acquisition_cancellation_destroy(&cancel);
  /* The hint is optional: without the host's bell, cursors still advance. */
  ft_track_notices_destroy(&h.notices);
  OK(write_block(&tb, 3));
  CHECK(ft_playback_doorbell_take(v.doorbell) == 0);
  ft_playback_reader *reader = NULL;
  OK(ft_playback_reader_create(v.rings[0], 30 * MS, &reader));
  uint64_t available, gaps, underruns;
  OK(ft_playback_reader_counters(reader, &available, &gaps, &underruns));
  ft_audio_block block;
  CHECK(available == 3 && read_block(reader, &block) && block.samples[0] == 1);
  ft_playback_reader_destroy(&reader);
  detach(&h, &v);
  discard(&ta);
  discard(&tb);
  return 0;
}

static int a_track_added_after_attach_is_selected_late(ft_local_connection *a, ft_local_connection *b) {
  track ta, tb;
  OK(create(1, &ta));
  OK(create(2, &tb));
  host h = {a, {{0}}, {NULL}, 0, NULL, 0};
  viewer v;
  CHECK(!attach(&h, b, NULL, 0, &v));
  ft_track_notice notice;
  CHECK(ft_tracks_next(v.tracks, 0, &notice) == FT_STATUS_EMPTY);
  /* A ring belongs only with a playback track, and playback needs one. */
  ft_track_offer track_a = offer(1, &ta, "default"), track_b = offer(2, &tb, "cue");
  ft_track_offer app = {3, "org.example.cue", "cue", (const uint8_t *)"\1", 1, FT_TRACK_FROM_SOURCE, 0, {{0}, 0, 0}};
  CHECK(ft_track_notices_add(h.notices, &track_a, NULL) == FT_STATUS_INVALID_ARGUMENT);
  CHECK(ft_track_notices_add(h.notices, &app, ta.playback) == FT_STATUS_INVALID_ARGUMENT);
  OK(ft_track_notices_add(h.notices, &track_a, ta.playback));
  /* A zero timeout polls: the notice already sent is returned. */
  OK(ft_tracks_next(v.tracks, 0, &notice));
  CHECK(notice.type == FT_TRACK_NOTICE_ADDED && notice.track.id == 1 && !strcmp(notice.track.label, "default") &&
        !memcmp(&notice.track.audio, &track_a.audio, sizeof track_a.audio));
  ft_track_admission admission;
  uint32_t position = 9;
  ft_playback_ring *ring_a = NULL, *ring_b = NULL, *none = NULL;
  ft_playback_doorbell *doorbell = NULL, *again = NULL;
  OK(select_late(&h, &v, selected(1), &admission, &position, &ring_a, &doorbell));
  /* The incarnation's first playback track brings the doorbell. */
  CHECK(admission.result == FT_TRACK_ADMITTED && position == 0 && ring_a && doorbell);
  ft_playback_reader *reader_a = NULL, *reader_b = NULL;
  OK(ft_playback_reader_create(ring_a, 250 * MS, &reader_a));
  OK(write_block(&ta, 1));
  /* Selecting before reading the notice queues it; a's reader is undisturbed. */
  OK(ft_track_notices_add(h.notices, &track_b, tb.playback));
  OK(select_late(&h, &v, selected(2), &admission, &position, &ring_b, &again));
  CHECK(admission.result == FT_TRACK_ADMITTED && position == 1 && ring_b && !again);
  OK(ft_tracks_next(v.tracks, 0, &notice));
  CHECK(notice.type == FT_TRACK_NOTICE_ADDED && notice.track.id == 2);
  /* An app-kind track is selected late without a ring; writing a
   * source-bound track is refused. */
  OK(ft_track_notices_add(h.notices, &app, NULL));
  ft_track_selection writing = {3, 0, 1};
  OK(select_late(&h, &v, writing, &admission, &position, &none, &again));
  CHECK(admission.result == FT_TRACK_REFUSED_NOT_WRITABLE);
  OK(select_late(&h, &v, selected(3), &admission, &position, &none, &again));
  CHECK(admission.result == FT_TRACK_ADMITTED && !none && !again);
  OK(ft_tracks_next(v.tracks, 0, &notice));
  CHECK(notice.track.id == 3 && notice.track.descriptor_len == 1 && notice.track.descriptor[0] == 1);
  OK(ft_playback_reader_create(ring_b, 250 * MS, &reader_b));
  OK(write_block(&ta, 2));
  OK(write_block(&tb, 1));
  ft_acquisition_cancellation *cancel = NULL;
  OK(ft_acquisition_cancellation_create(&cancel));
  uint64_t hint = 0;
  OK(ft_playback_doorbell_wait(doorbell, cancel, UINT64_MAX, &hint));
  CHECK(hint == 3);
  ft_audio_block block;
  CHECK(read_block(reader_a, &block) && block.cursor == 1);
  CHECK(read_block(reader_a, &block) && block.cursor == 2);
  CHECK(read_block(reader_b, &block) && block.samples[0] == 1);
  ft_acquisition_cancellation_destroy(&cancel);
  ft_playback_reader_destroy(&reader_a);
  ft_playback_reader_destroy(&reader_b);
  ft_playback_ring_destroy(&ring_a);
  ft_playback_ring_destroy(&ring_b);
  ft_playback_doorbell_destroy(&doorbell);
  detach(&h, &v);
  discard(&ta);
  discard(&tb);
  return 0;
}

static int a_reopen_updates_the_track_and_keeps_the_selection(ft_local_connection *a, ft_local_connection *b) {
  track t;
  host h;
  viewer v;
  CHECK(!attach_one(a, b, &t, &h, &v));
  ft_playback_reader *reader = NULL;
  OK(ft_playback_reader_create(v.rings[0], 250 * MS, &reader));
  OK(write_block(&t, 1));
  OK(ft_playback_writer_reconfigure(t.writer, &MONO));
  ft_track_offer reopened = offer(1, &t, "default");
  OK(ft_track_notices_update(h.notices, &reopened));
  ft_track_notice notice;
  OK(ft_tracks_next(v.tracks, 5000, &notice));
  CHECK(notice.type == FT_TRACK_NOTICE_UPDATED && !memcmp(&notice.track.audio.format, &MONO, sizeof MONO));
  OK(write_block(&t, 2));
  ft_audio_block block;
  CHECK(read_block(reader, &block) && block.flags == 0);
  CHECK(read_block(reader, &block) && block.flags == FT_AUDIO_BLOCK_RECONFIGURATION &&
        !memcmp(&block.format, &MONO, sizeof MONO));
  /* Still selected: selecting it again is refused. */
  ft_track_admission admission;
  uint32_t position;
  ft_playback_ring *ring = NULL;
  ft_playback_doorbell *doorbell = NULL;
  OK(select_late(&h, &v, selected(1), &admission, &position, &ring, &doorbell));
  CHECK(admission.result == FT_TRACK_REFUSED_UNAVAILABLE && !ring && !doorbell);
  /* A descriptor the ring cannot hold is not an update. */
  reopened.audio.blocks += 1;
  CHECK(ft_track_notices_update(h.notices, &reopened) == FT_STATUS_INVALID_ARGUMENT);
  ft_playback_reader_destroy(&reader);
  detach(&h, &v);
  discard(&t);
  return 0;
}

static int removal_ends_the_read_state_cleanly(ft_local_connection *a, ft_local_connection *b) {
  track t;
  host h;
  viewer v;
  CHECK(!attach_one(a, b, &t, &h, &v));
  ft_playback_reader *reader = NULL;
  OK(ft_playback_reader_create(v.rings[0], 250 * MS, &reader));
  ft_playback_ring_destroy(&v.rings[0]);
  OK(write_block(&t, 1));
  OK(write_block(&t, 2));
  ft_track_offer removed = offer(1, &t, "default");
  OK(ft_track_notices_remove(h.notices, 1));
  discard(&t);
  ft_track_notice notice;
  OK(ft_tracks_next(v.tracks, 5000, &notice));
  CHECK(notice.type == FT_TRACK_NOTICE_REMOVED && notice.track.id == 1 && !notice.track.kind);
  /* Blocks already written stay readable, then the ring is quiet. */
  ft_audio_block block;
  CHECK(read_block(reader, &block) && block.samples[0] == 1);
  CHECK(read_block(reader, &block) && block.samples[0] == 2);
  CHECK(ft_playback_reader_read(reader, &block) == FT_STATUS_EMPTY);
  ft_playback_reader_destroy(&reader);
  ft_track_admission admission;
  uint32_t position;
  ft_playback_ring *ring = NULL;
  ft_playback_doorbell *doorbell = NULL;
  OK(select_late(&h, &v, selected(1), &admission, &position, &ring, &doorbell));
  CHECK(admission.result == FT_TRACK_REFUSED_UNKNOWN);
  /* IDs are never reused. */
  track reused;
  OK(create(1, &reused));
  CHECK(ft_track_notices_add(h.notices, &removed, reused.playback) == FT_STATUS_INVALID_ARGUMENT);
  discard(&reused);
  /* The consumer's going ends the host's channel. */
  ft_tracks_destroy(&v.tracks);
  ft_local_connection_destroy(&v.media);
  ft_status served = ft_track_notices_serve(h.notices, 5000);
  CHECK(served != FT_STATUS_OK && served != FT_STATUS_EMPTY);
  detach(&h, &v);
  return 0;
}

typedef int (*scenario)(ft_local_connection *, ft_local_connection *);
static const scenario SCENARIOS[] = {
    offer_carries_the_descriptor_and_settings_floor_the_depth,
    writes_continue_without_readers_and_never_block,
    each_consumer_starts_at_cursor_minus_its_latency_target,
    underruns_are_counted_locally,
    a_lapped_reader_resynchronizes_with_a_gap,
    discontinuity_and_reconfiguration_mark_exact_samples,
    the_doorbell_hints_changed_tracks,
    a_track_added_after_attach_is_selected_late,
    a_reopen_updates_the_track_and_keeps_the_selection,
    removal_ends_the_read_state_cleanly,
};

/* Run scenario `index` on a connected pair: 0 on success, the failing line,
 * or -1 past the last scenario. */
int jackstay_c_playback_scenario(unsigned index, ft_local_connection *host, ft_local_connection *peer) {
  if (ft_abi_version() != FT_ABI_VERSION) return __LINE__;
  if (index >= sizeof SCENARIOS / sizeof SCENARIOS[0]) return -1;
  return SCENARIOS[index](host, peer);
}
