#ifndef CAPTURE_TRANSFER_H
#define CAPTURE_TRANSFER_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/*
 * ABI version this header describes, as (major << 16) | minor. Compare
 * against ft_abi_version() from the linked library at startup: a major
 * mismatch is incompatible; a larger library minor is compatible (additions
 * arrive as new functions and new attach-stream operations, never as changes
 * to existing struct layouts — those are pinned by the _Static_asserts
 * below).
 *
 * Major 0 means pre-stabilization: layouts may still change freely, with a
 * minor bump as the only signal. Require an exact version match at major 0. The 1.0 stamp waits until an external
 * consumer needs the stability promise.
 */
#define FT_ABI_VERSION_MAJOR 0
#define FT_ABI_VERSION_MINOR 9
#define FT_ABI_VERSION ((uint32_t)((FT_ABI_VERSION_MAJOR << 16) | FT_ABI_VERSION_MINOR))

uint32_t ft_abi_version(void);

#define FT_STATUS_OK 0
#define FT_STATUS_EMPTY 1
#define FT_STATUS_INVALID_ARGUMENT 2
#define FT_STATUS_ERROR 3
#define FT_STATUS_TIMEOUT 4
#define FT_STATUS_CLOSED 5
#define FT_STATUS_UNSUPPORTED 6
#define FT_STATUS_INVALID_STATE 7
#define FT_STATUS_HOLDING_LIMIT 8
#define FT_STATUS_RECONFIGURATION 9
#define FT_STATUS_MISS 10
#define FT_STATUS_GAP 11
#define FT_STATUS_CANCELLED 12
#define FT_STATUS_STALE 13
#define FT_STATUS_DRAINING 14
#define FT_STATUS_DROPPED 15
#define FT_STATUS_PAUSED_CAPACITY 16
#define FT_STATUS_CAPACITY 17
#define FT_STATUS_RECOVERY_REQUIRED 18
#define FT_STATUS_ADDRESS_IN_USE 19
#define FT_STATUS_UNTRUSTED_PEER 20

#define FT_SOURCE_KIND_WINDOW 1
#define FT_SOURCE_KIND_DISPLAY 2
#define FT_SOURCE_KIND_SURFACE 3

#define FT_TRACK_TYPE_VIDEO 1

#define FT_PIXEL_FORMAT_UNKNOWN 0
#define FT_PIXEL_FORMAT_BGRA8_UNORM 1
#define FT_PIXEL_FORMAT_RGBA8_UNORM 2

#define FT_CLOCK_DOMAIN_UNKNOWN 0
#define FT_CLOCK_DOMAIN_UNIX_TIME 1
#define FT_CLOCK_DOMAIN_MEDIA_TIME 2
#define FT_CLOCK_DOMAIN_HOST_TIME 3

#define FT_COLOR_SPACE_UNKNOWN 0
#define FT_COLOR_SPACE_SRGB 1

#define FT_FRAME_SYNC_UNKNOWN 0
#define FT_FRAME_SYNC_CPU_COPY_COMPLETE 1
#define FT_FRAME_SYNC_SCK_SAMPLE_READY 2
#define FT_FRAME_SYNC_NATIVE_TIMELINE 3

#define FT_DAMAGE_UNKNOWN 0
#define FT_DAMAGE_FULL_FRAME 1
#define FT_DAMAGE_NONE 2
#define FT_DAMAGE_INLINE_RECTS 3
#define FT_DAMAGE_SIDECAR_RECTS 4

#define FT_EVENT_PRODUCER_STARTED 1
#define FT_EVENT_SOURCE_REGISTERED 2
#define FT_EVENT_SOURCE_UPDATED 3
#define FT_EVENT_TRACK_REGISTERED 4
#define FT_EVENT_TRACK_UPDATED 5
#define FT_EVENT_SOURCE_UNREGISTERED 6
#define FT_EVENT_PRODUCER_STOPPED 7

typedef int32_t ft_status;
typedef uint64_t ft_source_id;
typedef uint64_t ft_track_id;

/* An owned OS setup object: a file descriptor on POSIX, a HANDLE on Windows.
 * Consumed slots are set to FT_OS_OBJECT_NONE. */
#if defined(_WIN32)
typedef void *ft_os_object;
#define FT_OS_OBJECT_NONE ((ft_os_object)0)
#else
typedef int32_t ft_os_object;
#define FT_OS_OBJECT_NONE ((ft_os_object)-1)
#endif

/* ABI 0.9 Local Endpoints (Wheelhouse ADR 0011): a logical address (scope,
 * name, transport kind) rendered per platform, never a caller-supplied path:
 * <runtime>/jackstay-<uid>/<name>.sock on POSIX, or
 * \\.\pipe\jackstay.<user SID>[.s<session>].<name> on Windows (named pipes with a
 * protected SYSTEM+user DACL, or the logon SID for SESSION scope; remote
 * clients rejected; first-instance creation). Names are 1..64 of [A-Za-z0-9._-]
 * and do not start with '.'. SESSION equals USER where there is no session.
 *
 * A host creates a listener and accepts connections carrying the kernel's view
 * of the peer (PID, user, Windows session); it still authorizes each peer and
 * selects the source. A client connects and the library verifies the server
 * runs as the current user (and session, for SESSION scope) before returning.
 * A connection is then consumed by exactly one *_local setup call: CPU setup,
 * source bootstrap (which hands it back for CPU setup) or input. These are the
 * Windows setup entry points; the fd-taking calls remain for POSIX callers.
 * Setup is blocking: run it off GUI/input threads. */
#define FT_ENDPOINT_SCOPE_USER 1u
#define FT_ENDPOINT_SCOPE_SESSION 2u
#define FT_ENDPOINT_TRANSPORT_LOCAL_STREAM 1u

typedef struct ft_local_endpoint {
  uint32_t scope;     /* FT_ENDPOINT_SCOPE_* */
  uint32_t transport; /* FT_ENDPOINT_TRANSPORT_LOCAL_STREAM; others UNSUPPORTED */
  const char *name;   /* UTF-8, copied during the call */
} ft_local_endpoint;

typedef struct ft_peer_identity {
  uint32_t pid;
  uint32_t session;     /* Windows session ID when has_session */
  uint32_t has_session;
  uint32_t reserved;
  char user[192];       /* NUL-terminated: user SID string, or decimal UID */
} ft_peer_identity;

typedef struct ft_local_listener ft_local_listener;
typedef struct ft_local_connection ft_local_connection;

/* The platform address, NUL-terminated, for diagnostics. CAPACITY: too short. */
ft_status ft_local_endpoint_render(const ft_local_endpoint *endpoint, char *out, size_t len);
/* ADDRESS_IN_USE: the endpoint (on Windows, any pipe of that name) exists; it
 * is never shared or taken over. *out starts NULL. */
ft_status ft_local_listener_create(const ft_local_endpoint *endpoint, ft_local_listener **out);
/* Blocks for one client. UNTRUSTED_PEER: the client could not be identified or
 * is in another Windows session on a SESSION endpoint; it was disconnected and
 * the listener stays usable. CANCELLED after cancel. Accept and cancel may
 * overlap; destroy only after both return. *out starts NULL. */
ft_status ft_local_listener_accept(const ft_local_listener *listener, ft_local_connection **out);
void ft_local_listener_cancel(const ft_local_listener *listener);
void ft_local_listener_destroy(ft_local_listener **listener);
/* Connects and verifies the server (UNTRUSTED_PEER otherwise). Waits up to five
 * seconds for a busy endpoint (TIMEOUT). *out starts NULL. */
ft_status ft_local_connect(const ft_local_endpoint *endpoint, ft_local_connection **out);
ft_status ft_local_connection_peer(const ft_local_connection *connection, ft_peer_identity *out);
/* OK while the peer holds its end, CLOSED after; never consumes bytes. */
ft_status ft_local_connection_alive(const ft_local_connection *connection);
/* Closes an unconsumed connection; NULL and *connection=NULL are harmless. */
void ft_local_connection_destroy(ft_local_connection **connection);


#if defined(__unix__) || defined(__APPLE__) || defined(_WIN32)
/* Common acquisition ownership (shared arena). Import a host's CPU grant,
 * or transfer an admitted Rust ArenaConsumer via FtAcquisitionConsumer::into_raw.
 * Serialize calls on a consumer. Frame handles are independent owners and may
 * outlive it. Never copy ownership, fork mappings, or call through stale handles.
 */
typedef struct ft_acquisition_consumer ft_acquisition_consumer;
typedef struct ft_acquired_frame ft_acquired_frame;
typedef struct ft_acquisition_cancellation ft_acquisition_cancellation;
typedef struct ft_acquisition_release_timeline ft_acquisition_release_timeline;

#define FT_ACQUIRE_LATEST 1
#define FT_ACQUIRE_NEXT 2
#define FT_ACQUIRE_EXACT 3
#define FT_WAIT_DATA 1
#define FT_WAIT_CAPACITY 2
#define FT_WAIT_ALL 3
#define FT_ACQUISITION_WAIT_INFINITE UINT64_MAX

/* Same repr(C) descriptor used by Rust FrameLease. Remains immutable and valid
 * even when publication history expires or a new configuration is installed. */
typedef struct ft_acquired_frame_descriptor {
  uint64_t cursor;
  uint64_t sequence;
  uint64_t timestamp_ns;
  uint64_t config_generation;
  uint64_t pool_id;
  uint64_t payload_offset;
  uint64_t payload_len;
  uint64_t modifier;
  uint64_t fence_id;
  uint64_t fence_value;
  uint64_t damage_base_sequence;
  uint64_t producer_drop_count;
  uint32_t width;
  uint32_t height;
  uint32_t stride;
  uint32_t pixel_format;
  uint32_t slot_id;
  uint32_t clock_domain;
  uint32_t color_space;
  uint32_t sync_kind;
  uint32_t payload_kind;
  uint32_t damage_kind;
  uint32_t dropped_before_publish;
  uint32_t flags;
} ft_acquired_frame_descriptor;

typedef struct ft_acquisition_range {
  uint64_t first;
  uint64_t last;
} ft_acquisition_range;

typedef struct ft_acquisition_events {
  uint64_t data_cursor;
  uint64_t capacity_epoch;
  uint64_t reconfiguration_epoch;
  uint32_t closed;
  uint32_t reserved;
} ft_acquisition_events;

/* Import GrantDescriptor JSON and its five owned setup objects (FDs; on Windows
 * HANDLEs owned by this process). The trusted producer must follow Jackstay's
 * shared-memory protocol and bind the grant to this PID. Grants are single-use:
 * no replay, forwarding, fork or retained transport copies. JSON length must be
 * 1..1048576; *out must start NULL. Invalid pointers, lengths, occupied outputs
 * or invalid/duplicate objects reject without transfer. Once these argument
 * checks pass, all five are consumed and set to FT_OS_OBJECT_NONE, including on
 * malformed JSON or failed mapping. Import CPU grants only; native resources
 * require the backend setup that retains their handles with leases. */
ft_status ft_acquisition_import_cpu(const uint8_t *json, size_t len, ft_os_object fds[5],
                                   ft_acquisition_consumer **out);
/* Install ConfigurationDescriptor JSON plus its single owned resource object on
 * this already admitted consumer. The same single-use, no-fork and no-extra-copy
 * rules apply. Invalid pointers, lengths or an invalid object reject without
 * transfer. After basic validation the object is consumed and set to
 * FT_OS_OBJECT_NONE on every outcome. OK installs it; STALE disposes a valid
 * superseded offer so the host can offer the current generation. Contradictory
 * mappings remain ERROR. Existing frame handles keep their original storage and
 * holding credit. */
ft_status ft_acquisition_install_cpu_configuration(ft_acquisition_consumer *,
                                                  const uint8_t *json, size_t len, ft_os_object *fd);
/* Drop the consumer's unleased current mapping, e.g. during a capacity pause.
 * Frame handles and deferred uses retain their own mappings; admission and
 * notification state survive. The host retries allocation once budget permits. */
ft_status ft_acquisition_relinquish_configuration(ft_acquisition_consumer *);

/* Latest/next select after cursor; exact selects cursor (zero is invalid).
 * *out must start NULL. Success owns one frame, including duplicate acquisitions.
 * MISS returns [cursor,cursor]; GAP returns an inclusive published range. Other
 * outcomes clear range. No non-success outcome transfers a frame. All pointer
 * arguments are required and must not alias. An occupied *out is rejected. */
ft_status ft_acquisition_acquire(const ft_acquisition_consumer *, uint32_t mode,
                                uint64_t cursor, ft_acquired_frame **out,
                                ft_acquisition_range *range);
ft_status ft_acquired_frame_describe(const ft_acquired_frame *, ft_acquired_frame_descriptor *out);
/* Borrow bytes through release or declared deferred completion. Satisfy producer
 * readiness before reading. Native frames return NULL,0; query native resources
 * through their backend binding. */
ft_status ft_acquired_frame_bytes(const ft_acquired_frame *, const uint8_t **data, size_t *len);
/* Immediate release asserts all use (including GPU work) has completed. */
ft_status ft_acquired_frame_release(ft_acquired_frame **);
/* Host supplies a registered, consumer-bound timeline. Success clears the frame
 * handle but keeps storage and credit until completion. Failure preserves the
 * exact handle and all ownership so the caller can retry. */
ft_status ft_acquired_frame_defer_release(ft_acquired_frame **,
                                         const ft_acquisition_release_timeline *, uint64_t value);
void ft_acquisition_release_timeline_destroy(ft_acquisition_release_timeline **);
/* Take snapshot BEFORE checking acquisition. Reconfiguration and closure always
 * wake. Cancellation takes precedence and never releases held frames.
 * Outputs must not alias inputs. timeout_ns=0 checks once, UINT64_MAX waits
 * indefinitely. OK/CLOSED supply a new snapshot; other outcomes clear it. */
ft_status ft_acquisition_snapshot(const ft_acquisition_consumer *, ft_acquisition_events *out);
ft_status ft_acquisition_wait(ft_acquisition_consumer *, const ft_acquisition_events *observed,
                             uint32_t interest, const ft_acquisition_cancellation *,
                             uint64_t timeout_ns, ft_acquisition_events *out);
ft_status ft_acquisition_cancellation_create(ft_acquisition_cancellation **out);
/* May run concurrently with wait; destroy only after all callers return. */
ft_status ft_acquisition_cancellation_cancel(const ft_acquisition_cancellation *);
void ft_acquisition_cancellation_destroy(ft_acquisition_cancellation **);
/* Destroys this API handle; acquired/deferred frames keep their own owners.
 * All destroy functions accept NULL or *handle=NULL and clear live handles. */
void ft_acquisition_consumer_destroy(ft_acquisition_consumer **);

#if defined(__APPLE__) || defined(__linux__) || defined(_WIN32)
typedef struct ft_cpu_acquisition_connection ft_cpu_acquisition_connection;
/* Generic setup on a connection from ft_local_connect (after an optional
 * ft_source_bootstrap_connect_local), whose server was verified. Same rules as
 * ft_acquisition_cpu_connection_create below: the peer must be the conforming
 * sole producer, no forwarding/replay of grants/maps, and no admission I/O until
 * attach. After basic checks *connection is consumed and set to NULL on every
 * outcome. On Windows the producer duplicates the grant's handles into this
 * verified process and this process acknowledges them. */
ft_status ft_acquisition_cpu_connection_create_local(ft_local_connection **connection,
                                                    ft_cpu_acquisition_connection **out);
/* Liveness of the producer's end of setup, never consuming setup bytes: OK while
 * it holds the connection (also while a setup call is in progress), CLOSED once
 * it closed, exited or setup failed, CANCELLED after cancel. Poll it between
 * frames; held frames stay valid. May overlap other calls except destroy. */
ft_status ft_acquisition_cpu_connection_alive(const ft_cpu_acquisition_connection *);
ft_status ft_acquisition_cpu_attach(ft_cpu_acquisition_connection *, uint32_t holding,
                                   ft_acquisition_consumer **out);
void ft_acquisition_cpu_connection_cancel(const ft_cpu_acquisition_connection *);
ft_status ft_acquisition_cpu_install_configuration(ft_cpu_acquisition_connection *, ft_acquisition_consumer *);
void ft_acquisition_cpu_connection_destroy(ft_cpu_acquisition_connection **);
#endif

#if defined(__APPLE__) || defined(__linux__)
/* Select a trusted host session, authorize it with an optional token and admit
 * the requested holding capacity. Strings are UTF-8 and copied during setup.
 * Both output handles start NULL; track is set on success. Never fork, forward
 * or replay these mappings. Connection, consumer and frames are separate owners. */
/* Generic setup on a connected, host-authorized Unix SOCK_STREAM. No daemon is
 * required. The peer must be the conforming sole producer; this process must be
 * the original peer and sole recipient of grants. No caller descriptor copies,
 * concurrent stream I/O, fork, forwarding or replay of grants/maps are allowed.
 * NULL arguments, negative fd or non-NULL *out leave fd unchanged. After these
 * basic checks, every outcome consumes fd and sets it to -1, even ERROR.
 * create performs no admission I/O; attach blocks and admits one consumer.
 * Attach/configuration protocol or admission failures return ERROR.
 * Serialize attach/configuration calls. cancel may run concurrently with them;
 * it permanently interrupts setup I/O, without releasing held frames. An already
 * completed operation keeps its result (including an admitted consumer or an
 * installed configuration); cancellation never rolls it back. A failed operation
 * interrupted by cancellation and subsequent setup calls return CANCELLED.
 * A private descriptor duplicate is used only for shutdown, never grant I/O.
 * All calls must return before connection destruction. */
ft_status ft_acquisition_cpu_connection_create(int32_t *fd, ft_cpu_acquisition_connection **out);
ft_status ft_acquisition_cpu_connect_session(const char *control_path, const char *session_id,
                                             const char *token, uint32_t holding,
                                             ft_cpu_acquisition_connection **out_connection,
                                             ft_acquisition_consumer **out_consumer, uint64_t *out_track);
#endif

#if defined(__APPLE__)
/* Requires a library built with backend-macos. The host-selected Mach service
 * must obey the common acquisition protocol; the optional client token does
 * not authenticate the producer. Both output handles must start NULL. Setup
 * connection and consumer are separate owners; serialize their operations. */
typedef struct ft_macos_acquisition_connection ft_macos_acquisition_connection;
ft_status ft_acquisition_macos_connect(const char *endpoint, const char *token, uint32_t holding,
                                       ft_macos_acquisition_connection **out_connection,
                                       ft_acquisition_consumer **out_consumer);
/* Request and install one native replacement. OK installed, EMPTY no offer,
 * STALE disposed a valid superseded offer. Old frames retain their own handles. */
ft_status ft_acquisition_macos_install_configuration(ft_macos_acquisition_connection *,
                                                    ft_acquisition_consumer *);
/* Borrow the consumer's actual MTLSharedEventHandle during registration. The
 * library imports an independent observer and returns a common release binding.
 * The caller keeps ownership of event_handle; *out must initially be NULL. */
ft_status ft_acquisition_macos_register_release(ft_macos_acquisition_connection *,
                                               const ft_acquisition_consumer *, void *event_handle,
                                               ft_acquisition_release_timeline **out);
/* Borrow IOSurfaceRef + MTLSharedEventHandle from this acquired generation.
 * Outputs must not alias and are cleared on non-success. UNSUPPORTED means the
 * frame has no macOS native resources. The frame descriptor supplies dimensions,
 * format and the producer fence value to wait before sampling. Finish use and
 * dispose imported handle copies within the frame's lease, including its
 * declared deferred completion. No pool cache is needed. */
ft_status ft_acquired_frame_macos_resources(const ft_acquired_frame *, void **out_surface,
                                           void **out_readiness);
/* Close setup without declaring outstanding work complete; clears the handle.
 * Consumer/frame handles have their own lifetimes. NULL is harmless. */
void ft_acquisition_macos_connection_destroy(ft_macos_acquisition_connection **);
#endif

/* Single-stream CPU producer. Source/track selection belongs to
 * the host. Serialize producer calls; consumer/frame lifetimes are independent.
 * Local handles/maps must not be forked or forwarded into another process. */
typedef struct ft_cpu_producer ft_cpu_producer;
typedef struct ft_cpu_producer_config {
  uint32_t resource_capacity, retained_history, producer_reserve, max_incarnations;
  uint64_t payload_capacity, memory_budget, drain_timeout_ns;
} ft_cpu_producer_config;
typedef struct ft_cpu_reconfiguration {
  uint64_t generation, requested_bytes, available_bytes;
} ft_cpu_reconfiguration;

/* Handle outputs start NULL and are set only on success. No hidden defaults:
 * config includes all resource, byte-budget and incarnation limits. */
ft_status ft_cpu_producer_create(const ft_cpu_producer_config *, ft_cpu_producer **out);
ft_status ft_cpu_producer_attach(ft_cpu_producer *, uint32_t holding, ft_acquisition_consumer **out);
/* Input bytes are copied before return; descriptor/output/bytes are disjoint.
 * Only BGRA/RGBA CPU frames with consistent dimensions/stride/length are accepted.
 * The copy stamps CPU readiness and clears native fences/modifier. Arena identity,
 * cursor, payload location and configuration generation are stamped by publication.
 * OK returns a nonzero cursor; DROPPED returns zero without acquiring storage. */
ft_status ft_cpu_producer_publish(ft_cpu_producer *, const ft_acquired_frame_descriptor *,
                                  const uint8_t *bytes, size_t len, uint64_t *out_cursor);
/* PAUSED_CAPACITY reports the pending transition's overlap budget. CAPACITY
 * rejects a proposal that cannot fit after old resources retire. */
ft_status ft_cpu_producer_reconfigure(ft_cpu_producer *, uint64_t payload_capacity, ft_cpu_reconfiguration *out);
ft_status ft_cpu_producer_advance(ft_cpu_producer *, ft_cpu_reconfiguration *out);
ft_status ft_cpu_producer_configure_consumer(ft_cpu_producer *, ft_acquisition_consumer *);
ft_status ft_cpu_producer_poll_cleanup(ft_cpu_producer *);
/* Stops acquisition/publication. DRAINING/RECOVERY_REQUIRED leave the producer
 * handle owned by the caller: release work, continue maintenance and retry.
 * Stop/join setup workers too: they retain the producer even before admission.
 * Only OK destroys/clears it; timeout never permits forced reclamation. */
ft_status ft_cpu_producer_destroy(ft_cpu_producer **);

#if defined(__APPLE__) || defined(__linux__) || defined(_WIN32)
typedef struct ft_cpu_setup_server ft_cpu_setup_server;
/* One setup worker for one connection the host accepted from a Local Endpoint
 * listener and authorized. Admission binds to that connection's verified peer
 * process; on Windows grants are duplicated into it and acknowledged. After
 * basic checks *connection is consumed and set to NULL on every outcome. Same
 * worker, poll, cancel and destroy semantics as ft_cpu_producer_serve below. */
ft_status ft_cpu_producer_serve_local(ft_cpu_producer *, ft_local_connection **connection,
                                      ft_cpu_setup_server **out);
void ft_cpu_setup_server_cancel(const ft_cpu_setup_server *);
ft_status ft_cpu_setup_server_poll(ft_cpu_setup_server *);
ft_status ft_cpu_setup_server_destroy(ft_cpu_setup_server **);
#endif

#if defined(__APPLE__) || defined(__linux__)
/* One setup worker for one connected, host-authorized Unix SOCK_STREAM. Host
 * owns listening, authorization and source selection. Same FD-transfer rules as
 * connection_create above. OK means worker started, not consumer admitted.
 * Internal arena locking permits the worker to serve alongside serialized host
 * producer calls; socket I/O never holds that lock. poll is nonblocking: DRAINING
 * while running, OK after orderly EOF, ERROR after setup/protocol failure,
 * CANCELLED after explicit cancellation. cancel may run concurrently with poll.
 * destroy cancels a running worker, joins it, clears the handle and returns its
 * final status. No other call may overlap destruction. It can wait for a finite
 * arena operation; shutdown interrupts socket I/O. Close stops new acquisitions;
 * existing frame ownership survives. EOF is not proof of peer process death. */
ft_status ft_cpu_producer_serve(ft_cpu_producer *, int32_t *fd, ft_cpu_setup_server **out);
#endif

#if defined(__STDC_VERSION__) && __STDC_VERSION__ >= 201112L
_Static_assert(sizeof(ft_acquired_frame_descriptor) == 144, "acquired descriptor size");
_Static_assert(offsetof(ft_acquired_frame_descriptor, fence_value) == 72, "acquired readiness packing");
_Static_assert(offsetof(ft_acquired_frame_descriptor, width) == 96, "acquired dimensions packing");
_Static_assert(offsetof(ft_acquired_frame_descriptor, flags) == 140, "acquired flags packing");
_Static_assert(sizeof(ft_acquisition_range) == 16, "acquisition range size");
_Static_assert(sizeof(ft_acquisition_events) == 32, "acquisition events size");
_Static_assert(sizeof(ft_cpu_producer_config) == 40, "CPU producer config size");
_Static_assert(sizeof(ft_cpu_reconfiguration) == 24, "CPU reconfiguration size");
#endif
#endif

#if defined(__STDC_VERSION__) && __STDC_VERSION__ >= 201112L && UINTPTR_MAX == UINT64_MAX
_Static_assert(sizeof(ft_local_endpoint) == 16, "local endpoint size");
_Static_assert(offsetof(ft_local_endpoint, name) == 8, "local endpoint packing");
_Static_assert(sizeof(ft_peer_identity) == 208, "peer identity size");
#endif

typedef struct ft_synthetic_session {
  char session_id[64];
  ft_source_id source_id;
  ft_track_id track_id;
  char fd_socket_path[4096];
} ft_synthetic_session;

ft_status ft_create_synthetic_session(const char *control_socket_path,
                                      ft_synthetic_session *out);

/* Native handle path. This is intentionally a low-level C ABI:
 *
 * - Every struct with a struct_size field must be initialized by the caller to
 *   sizeof(that struct) before passing it to a ft_native_* function. Output
 *   structs are validated the same way before they are overwritten.
 * - Handles exposed through grants and pools are borrowed from the
 *   ft_native_attach and stay valid until ft_native_attach_destroy or a later
 *   reconfiguration contract retires them after outstanding leases drain.
 * - Consumers acquire frame leases explicitly and must release each successful
 *   acquire with ft_native_release_frame before the producer can safely reuse
 *   that slot.
 */
typedef struct ft_native_attach ft_native_attach;

#define FT_WAIT_INFINITE UINT64_MAX

#define FT_NATIVE_ATTACH_TRANSPORT_MACOS_XPC 1
#define FT_NATIVE_ATTACH_TRANSPORT_UNIX_SOCKET 2

#define FT_NATIVE_HANDLE_IOSURFACE 1
#define FT_NATIVE_HANDLE_DMABUF 2
/* NT handle of a shared (SHARED_NTHANDLE) BGRA8 ID3D11Texture2D. */
#define FT_NATIVE_HANDLE_D3D11_TEXTURE 3
#define FT_NATIVE_MAX_PLANES 4

#define FT_NATIVE_SYNC_NONE 0
#define FT_NATIVE_SYNC_MTL_SHARED_EVENT 1
#define FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE 2
/* NT handle of a shared ID3D11Fence timeline. */
#define FT_NATIVE_SYNC_D3D11_FENCE 3

#define FT_NATIVE_RELEASE_NOW 1
#define FT_NATIVE_RELEASE_TIMELINE_VALUE 2

#define FT_NATIVE_EVENT_POOL_ADDED 1
#define FT_NATIVE_EVENT_POOL_REMOVED 2
#define FT_NATIVE_EVENT_STREAM_CONFIG_CHANGED 3
#define FT_NATIVE_EVENT_PRODUCER_STOPPED 4

typedef struct ft_native_attach_descriptor {
  uint32_t struct_size;
  uint32_t transport_kind;
  uint64_t requested_consumer_id; /* 0 = assign */
  const char *endpoint;
  const char *bearer_token;
  uint32_t flags; /* must be 0 in this ABI revision */
} ft_native_attach_descriptor;

typedef struct ft_native_plane {
  int32_t fd;
  uint32_t offset;
  uint32_t stride;
} ft_native_plane;

typedef struct ft_native_surface {
  uint32_t struct_size;
  uint32_t handle_kind;
  uint32_t plane_count;
  uint32_t width;
  uint32_t height;
  uint32_t pixel_format;
  uint64_t modifier;
  void *object; /* IOSurfaceRef / D3D resource wrapper; NULL for dmabuf */
  ft_native_plane planes[FT_NATIVE_MAX_PLANES];
} ft_native_surface;

typedef union ft_native_sync_handle {
  void *object;
  int32_t fd;
} ft_native_sync_handle;

typedef struct ft_native_sync {
  uint32_t struct_size;
  uint32_t sync_kind;
  uint64_t sync_id;
  ft_native_sync_handle handle;
} ft_native_sync;

typedef struct ft_native_pool {
  uint32_t struct_size;
  uint32_t surface_count;
  uint64_t pool_id;
  const ft_native_surface *surfaces;
} ft_native_pool;

typedef struct ft_native_grant {
  uint32_t struct_size;
  uint32_t pool_count;
  uint64_t consumer_id;
  uint64_t consumer_slot;
  const ft_native_pool *pools;
  ft_native_sync producer_sync;
} ft_native_grant;

typedef struct ft_native_frame {
  uint32_t struct_size;
  uint32_t flags;
  uint64_t lease_id;
  uint64_t cursor;
  uint64_t sequence;
  uint64_t timestamp_ns;
  uint64_t pool_id;
  uint32_t slot_id;
  uint32_t width;
  uint32_t height;
  uint32_t pixel_format;
  uint64_t producer_sync_id;
  uint64_t producer_sync_value; /* GPU-wait this before sampling */
} ft_native_frame;

typedef struct ft_native_release {
  uint32_t struct_size;
  uint32_t release_kind;
  uint64_t lease_id;
  uint64_t release_sync_id;
  uint64_t release_value;
} ft_native_release;

typedef struct ft_native_event {
  uint32_t struct_size;
  uint32_t kind;
  uint64_t pool_id;
  uint64_t config_generation;
} ft_native_event;

#if defined(__STDC_VERSION__) && __STDC_VERSION__ >= 201112L
/* Pin the native ABI; mirror is the #[repr(C)] block in src/ffi_native.rs.
 * The sizes below assume 8-byte pointers (pointer-bearing structs like
 * ft_native_sync and ft_native_grant shrink under ILP32). Declare the
 * constraint explicitly so a 32-bit build fails with one clear message
 * instead of a wall of packing asserts. */
_Static_assert(sizeof(void *) == 8, "jackstay native ABI assumes 64-bit pointers (no ILP32 target yet)");
_Static_assert(sizeof(ft_native_attach_descriptor) == 40, "native attach descriptor size");
_Static_assert(offsetof(ft_native_attach_descriptor, requested_consumer_id) == 8, "native attach descriptor packing");
_Static_assert(offsetof(ft_native_attach_descriptor, endpoint) == 16, "native attach descriptor packing");
_Static_assert(offsetof(ft_native_attach_descriptor, bearer_token) == 24, "native attach descriptor packing");
_Static_assert(offsetof(ft_native_attach_descriptor, flags) == 32, "native attach descriptor packing");
_Static_assert(sizeof(ft_native_plane) == 12, "native plane size");
_Static_assert(sizeof(ft_native_surface) == 88, "native surface size");
_Static_assert(offsetof(ft_native_surface, object) == 32, "native surface packing");
_Static_assert(sizeof(ft_native_sync) == 24, "native sync size");
_Static_assert(sizeof(ft_native_pool) == 24, "native pool size");
_Static_assert(sizeof(ft_native_grant) == 56, "native grant size");
_Static_assert(offsetof(ft_native_grant, pools) == 24, "native grant packing");
_Static_assert(offsetof(ft_native_grant, producer_sync) == 32, "native grant packing");
_Static_assert(sizeof(ft_native_frame) == 80, "native frame size");
_Static_assert(offsetof(ft_native_frame, lease_id) == 8, "native frame packing");
_Static_assert(offsetof(ft_native_frame, producer_sync_id) == 64, "native frame packing");
_Static_assert(sizeof(ft_native_release) == 32, "native release size");
_Static_assert(sizeof(ft_native_event) == 24, "native event size");
#endif

ft_status ft_native_attach_connect(const ft_native_attach_descriptor *descriptor,
                                   ft_native_attach **out);
ft_status ft_native_attach_grant(const ft_native_attach *attach, ft_native_grant *out_grant);
ft_status ft_native_wait_frame(ft_native_attach *attach,
                               uint64_t min_cursor,
                               uint64_t timeout_ns,
                               uint64_t *out_cursor);
ft_status ft_native_acquire_latest(ft_native_attach *attach,
                                   uint64_t min_cursor,
                                   ft_native_frame *out_frame);
// Registers a release timeline for FT_NATIVE_RELEASE_TIMELINE_VALUE.
// Currently supported only for Linux DRM syncobj timeline handles; other
// platforms return FT_STATUS_UNSUPPORTED.
ft_status ft_native_register_release_sync(ft_native_attach *attach,
                                          const ft_native_sync *sync,
                                          uint64_t *out_release_sync_id);
ft_status ft_native_release_frame(ft_native_attach *attach,
                                  const ft_native_release *release);
ft_status ft_native_poll_event(ft_native_attach *attach, ft_native_event *out_event);
ft_status ft_native_get_pool(ft_native_attach *attach, uint64_t pool_id, ft_native_pool *out_pool);
void ft_native_attach_destroy(ft_native_attach *attach);

#ifdef __cplusplus
}
#endif

#endif
