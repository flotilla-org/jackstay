// macOS XPC attach transport shim (ADR-0007): the setup channel that moves
// what byte streams cannot. IOSurfaces and the MTLSharedEventHandle cross
// the connection AS OBJECTS inside NSXPC messages — they refuse plain
// serialization, and global IOSurfaceID lookup fails across the launchd ↔
// user-session boundary, so this live connection is the only road.
//
// The protocol logic (ordering, authorization, exactly-once) lives in Rust
// (native/attach.rs); this shim is the dumb pipe: decode a request, call
// the Rust callback, encode the reply.
//
// Conventions match the other shims: malloc'd error strings (NULL on
// success), out-params, +1 retained objects across the C boundary.

#import <Foundation/Foundation.h>
#import <IOSurface/IOSurface.h>
#import <IOSurface/IOSurfaceObjC.h> // the IOSurface ObjC class (NSXPC-encodable)
#import <Metal/Metal.h>

#include <stdint.h>
#include <string.h>

static char *porthole_xpc_copy_error(NSString *message) {
  const char *utf8 = message.UTF8String;
  if (utf8 == NULL) {
    utf8 = "unknown XPC transport error";
  }
  char *copy = malloc(strlen(utf8) + 1);
  if (copy == NULL) {
    abort();
  }
  strcpy(copy, utf8);
  return copy;
}

// ---- Wire protocol ---------------------------------------------------------

// Status codes mirror AttachError in native/attach.rs; 0 is success and
// negative values are transport-level failures.
@protocol PortholeAttachXpc
- (void)authorizeWithToken:(NSString *)token reply:(void (^)(int32_t status))reply;
- (void)attachWithConsumerId:(uint64_t)consumerId
                       reply:(void (^)(int32_t status,
                                       uint64_t consumerSlot,
                                       NSFileHandle *ringFd,
                                       uint64_t ringMapLen,
                                       uint64_t poolId,
                                       NSArray *surfaces,
                                       uint64_t fenceId,
                                       MTLSharedEventHandle *syncHandle))reply;
@end

static NSXPCInterface *porthole_attach_interface(void) {
  NSXPCInterface *interface = [NSXPCInterface interfaceWithProtocol:@protocol(PortholeAttachXpc)];
  SEL attach = @selector(attachWithConsumerId:reply:);
  [interface setClasses:[NSSet setWithObject:[NSFileHandle class]] forSelector:attach argumentIndex:2 ofReply:YES];
  [interface setClasses:[NSSet setWithObjects:[NSArray class], [IOSurface class], nil]
            forSelector:attach
          argumentIndex:5
                ofReply:YES];
  [interface setClasses:[NSSet setWithObject:[MTLSharedEventHandle class]] forSelector:attach argumentIndex:7 ofReply:YES];
  return interface;
}

// ---- Server ----------------------------------------------------------------

// The grant as Rust assembles it. Everything is borrowed from Rust for the
// duration of the attach callback; the shim bridges into ObjC objects (which
// take their own references) and then asks Rust to release via the grant
// release callback, still inside the message handler.
typedef struct PortholeXpcGrant {
  uint64_t consumer_slot;
  int32_t ring_fd; // dup'd by Rust; the shim's NSFileHandle takes ownership
  uint64_t ring_map_len;
  uint64_t pool_id;
  uint32_t surface_count;
  void *const *surfaces; // IOSurfaceRefs, borrowed
  uint64_t fence_id;
  void *sync_handle; // MTLSharedEventHandle, borrowed
} PortholeXpcGrant;

typedef int32_t (*PortholeXpcAuthorizeCallback)(void *ctx, uint64_t sessionId, const char *token);
typedef int32_t (*PortholeXpcAttachCallback)(void *ctx, uint64_t sessionId, uint64_t consumerId, PortholeXpcGrant *outGrant);
typedef void (*PortholeXpcGrantReleaseCallback)(void *ctx, uint64_t sessionId, PortholeXpcGrant *grant);
typedef void (*PortholeXpcSessionEndedCallback)(void *ctx, uint64_t sessionId);
typedef void (*PortholeXpcStateReleaseCallback)(void *ctx);

@class PortholeAttachListenerDelegate;

@interface PortholeAttachSession : NSObject <PortholeAttachXpc>
// Sessions strongly retain the delegate (and in-flight messages retain the
// session), so the delegate — and therefore the Rust ctx it releases on
// dealloc — outlives every message that could touch ctx.
@property(nonatomic, strong) PortholeAttachListenerDelegate *owner;
@property(nonatomic, assign) uint64_t sessionId;
@end

@interface PortholeAttachListenerDelegate : NSObject <NSXPCListenerDelegate>
@property(nonatomic, assign) void *ctx;
@property(nonatomic, assign) PortholeXpcAuthorizeCallback authorizeCallback;
@property(nonatomic, assign) PortholeXpcAttachCallback attachCallback;
@property(nonatomic, assign) PortholeXpcGrantReleaseCallback grantReleaseCallback;
@property(nonatomic, assign) PortholeXpcSessionEndedCallback sessionEndedCallback;
@property(nonatomic, assign) PortholeXpcStateReleaseCallback stateReleaseCallback;
@property(nonatomic, strong) NSMutableSet<NSXPCConnection *> *connections;
@property(atomic, assign) uint64_t nextSessionId;
@end

@implementation PortholeAttachSession
- (void)authorizeWithToken:(NSString *)token reply:(void (^)(int32_t))reply {
  const char *utf8 = token.UTF8String;
  reply(self.owner.authorizeCallback(self.owner.ctx, self.sessionId, utf8 != NULL ? utf8 : ""));
}

- (void)attachWithConsumerId:(uint64_t)consumerId
                       reply:(void (^)(int32_t, uint64_t, NSFileHandle *, uint64_t, uint64_t, NSArray *, uint64_t,
                                       MTLSharedEventHandle *))reply {
  PortholeXpcGrant grant = {0};
  int32_t status = self.owner.attachCallback(self.owner.ctx, self.sessionId, consumerId, &grant);
  if (status != 0) {
    reply(status, 0, nil, 0, 0, nil, 0, nil);
    return;
  }
  NSFileHandle *ringFd = [[NSFileHandle alloc] initWithFileDescriptor:grant.ring_fd closeOnDealloc:YES];
  NSMutableArray *surfaces = [NSMutableArray arrayWithCapacity:grant.surface_count];
  for (uint32_t i = 0; i < grant.surface_count; i++) {
    [surfaces addObject:(__bridge IOSurface *)grant.surfaces[i]];
  }
  MTLSharedEventHandle *syncHandle = (__bridge MTLSharedEventHandle *)grant.sync_handle;
  // The ObjC objects above hold their own references now; let Rust free the
  // grant's allocations before the reply is encoded.
  self.owner.grantReleaseCallback(self.owner.ctx, self.sessionId, &grant);
  reply(0, grant.consumer_slot, ringFd, grant.ring_map_len, grant.pool_id, surfaces, grant.fence_id, syncHandle);
}
@end

@implementation PortholeAttachListenerDelegate
- (instancetype)init {
  if ((self = [super init])) {
    _connections = [NSMutableSet set];
  }
  return self;
}

- (BOOL)listener:(NSXPCListener *)listener shouldAcceptNewConnection:(NSXPCConnection *)connection {
  (void)listener;
  uint64_t sessionId;
  @synchronized(self) {
    sessionId = ++self->_nextSessionId;
    [self.connections addObject:connection];
  }
  PortholeAttachSession *session = [PortholeAttachSession new];
  session.owner = self;
  session.sessionId = sessionId;

  connection.exportedInterface = porthole_attach_interface();
  connection.exportedObject = session;
  __weak PortholeAttachListenerDelegate *weakSelf = self;
  __weak NSXPCConnection *weakConnection = connection;
  connection.invalidationHandler = ^{
    PortholeAttachListenerDelegate *strongSelf = weakSelf;
    if (strongSelf == nil) {
      return;
    }
    @synchronized(strongSelf) {
      NSXPCConnection *strongConnection = weakConnection;
      if (strongConnection != nil) {
        [strongSelf.connections removeObject:strongConnection];
      }
    }
    if (strongSelf.sessionEndedCallback != NULL) {
      strongSelf.sessionEndedCallback(strongSelf.ctx, sessionId);
    }
  };
  [connection resume];
  return YES;
}

- (void)dealloc {
  // Last reference gone: no listener, no session, no in-flight message can
  // reach ctx any more. Hand it back to Rust.
  if (_stateReleaseCallback != NULL) {
    _stateReleaseCallback(_ctx);
  }
}
@end

@interface PortholeAttachListenerBox : NSObject
@property(nonatomic, strong) NSXPCListener *listener;
@property(nonatomic, strong) PortholeAttachListenerDelegate *delegate;
@end
@implementation PortholeAttachListenerBox
@end

// machServiceName == NULL creates an anonymous listener (its endpoint can be
// handed to a same-process or already-connected peer); otherwise the name
// must be registered for this process in launchd's MachServices.
//
// Ownership of `ctx` passes to the shim: it is released via
// stateReleaseCallback when the delegate deallocates, which by construction
// is after the listener is stopped, every connection is invalidated, and
// every in-flight message has drained.
char *porthole_xpc_listener_start(const char *machServiceName,
                                  PortholeXpcAuthorizeCallback authorizeCallback,
                                  PortholeXpcAttachCallback attachCallback,
                                  PortholeXpcGrantReleaseCallback grantReleaseCallback,
                                  PortholeXpcSessionEndedCallback sessionEndedCallback,
                                  PortholeXpcStateReleaseCallback stateReleaseCallback,
                                  void *ctx,
                                  void **outListener) {
  *outListener = NULL;
  PortholeAttachListenerDelegate *delegate = [PortholeAttachListenerDelegate new];
  delegate.ctx = ctx;
  delegate.authorizeCallback = authorizeCallback;
  delegate.attachCallback = attachCallback;
  delegate.grantReleaseCallback = grantReleaseCallback;
  delegate.sessionEndedCallback = sessionEndedCallback;
  delegate.stateReleaseCallback = stateReleaseCallback;

  NSXPCListener *listener;
  if (machServiceName == NULL) {
    listener = [NSXPCListener anonymousListener];
  } else {
    listener = [[NSXPCListener alloc] initWithMachServiceName:[NSString stringWithUTF8String:machServiceName]];
  }
  listener.delegate = delegate;
  [listener resume];

  PortholeAttachListenerBox *box = [PortholeAttachListenerBox new];
  box.listener = listener;
  box.delegate = delegate;
  *outListener = (__bridge_retained void *)box;
  return NULL;
}

void porthole_xpc_listener_stop(void *listenerPtr) {
  PortholeAttachListenerBox *box = (__bridge_transfer PortholeAttachListenerBox *)listenerPtr;
  [box.listener invalidate];
  NSSet<NSXPCConnection *> *connections;
  @synchronized(box.delegate) {
    connections = [box.delegate.connections copy];
  }
  for (NSXPCConnection *connection in connections) {
    [connection invalidate];
  }
}

// Returns the anonymous listener's endpoint, +1 retained. Only meaningful
// for same-process hand-off or shuttling over an existing XPC connection.
void *porthole_xpc_listener_copy_endpoint(void *listenerPtr) {
  PortholeAttachListenerBox *box = (__bridge PortholeAttachListenerBox *)listenerPtr;
  return (__bridge_retained void *)box.listener.endpoint;
}

// ---- Client ----------------------------------------------------------------

@interface PortholeAttachClientBox : NSObject
@property(nonatomic, strong) NSXPCConnection *connection;
@end
@implementation PortholeAttachClientBox
@end

static char *porthole_xpc_client_finish(NSXPCConnection *connection, void **outClient) {
  connection.remoteObjectInterface = porthole_attach_interface();
  [connection resume];
  PortholeAttachClientBox *box = [PortholeAttachClientBox new];
  box.connection = connection;
  *outClient = (__bridge_retained void *)box;
  return NULL;
}

char *porthole_xpc_client_connect_name(const char *machServiceName, void **outClient) {
  *outClient = NULL;
  NSXPCConnection *connection =
      [[NSXPCConnection alloc] initWithMachServiceName:[NSString stringWithUTF8String:machServiceName] options:0];
  if (connection == nil) {
    return porthole_xpc_copy_error(@"failed to create XPC connection to mach service");
  }
  return porthole_xpc_client_finish(connection, outClient);
}

char *porthole_xpc_client_connect_endpoint(void *endpointPtr, void **outClient) {
  *outClient = NULL;
  NSXPCListenerEndpoint *endpoint = (__bridge NSXPCListenerEndpoint *)endpointPtr;
  NSXPCConnection *connection = [[NSXPCConnection alloc] initWithListenerEndpoint:endpoint];
  if (connection == nil) {
    return porthole_xpc_copy_error(@"failed to create XPC connection from endpoint");
  }
  return porthole_xpc_client_finish(connection, outClient);
}

void porthole_xpc_client_destroy(void *clientPtr) {
  PortholeAttachClientBox *box = (__bridge_transfer PortholeAttachClientBox *)clientPtr;
  [box.connection invalidate];
}

void porthole_xpc_endpoint_release(void *endpointPtr) {
  (void)(__bridge_transfer NSXPCListenerEndpoint *)endpointPtr;
}

// Synchronous authorize. Returns the protocol status, or -1 with *outError
// set on a transport failure.
int32_t porthole_xpc_client_authorize(void *clientPtr, const char *token, char **outError) {
  PortholeAttachClientBox *box = (__bridge PortholeAttachClientBox *)clientPtr;
  *outError = NULL;
  __block int32_t status = -1;
  __block NSString *failure = nil;
  id<PortholeAttachXpc> proxy = [box.connection synchronousRemoteObjectProxyWithErrorHandler:^(NSError *error) {
    failure = error.localizedDescription;
  }];
  [proxy authorizeWithToken:[NSString stringWithUTF8String:token]
                      reply:^(int32_t replyStatus) {
                        status = replyStatus;
                      }];
  if (failure != nil) {
    *outError = porthole_xpc_copy_error(failure);
    return -1;
  }
  return status;
}

// Synchronous attach. On status 0 the out-params carry the grant: a dup of
// the ring fd, a malloc'd array of +1 retained IOSurfaceRefs (caller frees
// the array with porthole_xpc_surface_array_free and releases each surface),
// and the +1 retained sync handle.
int32_t porthole_xpc_client_attach(void *clientPtr,
                                   uint64_t consumerId,
                                   uint64_t *outConsumerSlot,
                                   int32_t *outRingFd,
                                   uint64_t *outRingMapLen,
                                   uint64_t *outPoolId,
                                   void ***outSurfaces,
                                   uint32_t *outSurfaceCount,
                                   uint64_t *outFenceId,
                                   void **outSyncHandle,
                                   char **outError) {
  PortholeAttachClientBox *box = (__bridge PortholeAttachClientBox *)clientPtr;
  *outError = NULL;
  *outSurfaces = NULL;
  *outSurfaceCount = 0;
  *outRingFd = -1;
  *outSyncHandle = NULL;
  __block int32_t status = -1;
  __block NSString *failure = nil;
  id<PortholeAttachXpc> proxy = [box.connection synchronousRemoteObjectProxyWithErrorHandler:^(NSError *error) {
    failure = error.localizedDescription;
  }];
  [proxy attachWithConsumerId:consumerId
                        reply:^(int32_t replyStatus, uint64_t consumerSlot, NSFileHandle *ringFd, uint64_t ringMapLen,
                                uint64_t poolId, NSArray *surfaces, uint64_t fenceId, MTLSharedEventHandle *syncHandle) {
                          status = replyStatus;
                          if (replyStatus != 0) {
                            return;
                          }
                          *outConsumerSlot = consumerSlot;
                          // NSFileHandle closes its descriptor on dealloc;
                          // dup so Rust owns an independent fd.
                          *outRingFd = ringFd != nil ? dup(ringFd.fileDescriptor) : -1;
                          *outRingMapLen = ringMapLen;
                          *outPoolId = poolId;
                          uint32_t count = (uint32_t)surfaces.count;
                          void **array = count > 0 ? malloc(sizeof(void *) * count) : NULL;
                          for (uint32_t i = 0; i < count; i++) {
                            array[i] = (void *)CFBridgingRetain(surfaces[i]);
                          }
                          *outSurfaces = array;
                          *outSurfaceCount = count;
                          *outFenceId = fenceId;
                          *outSyncHandle = (__bridge_retained void *)syncHandle;
                        }];
  if (failure != nil) {
    *outError = porthole_xpc_copy_error(failure);
    return -1;
  }
  if (status == 0 && *outRingFd < 0) {
    *outError = porthole_xpc_copy_error(@"attach reply carried no usable ring fd");
    return -1;
  }
  return status;
}

// Frees only the malloc'd pointer array; each IOSurface's +1 retain has
// already been adopted by the caller (IoSurface::from_retained) before this
// is called, so the surfaces themselves are not released here.
void porthole_xpc_surface_array_free(void **surfaces) {
  free(surfaces);
}
