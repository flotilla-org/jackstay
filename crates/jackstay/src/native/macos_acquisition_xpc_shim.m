// Acquisition setup only. Frame acquisition and release use shared mappings.
// The peer PID comes from NSXPCConnection, never from request metadata.
#import <Foundation/Foundation.h>
#import <IOSurface/IOSurface.h>
#import <IOSurface/IOSurfaceObjC.h>
#import <Metal/Metal.h>
#include <unistd.h>
#include <string.h>

@interface JSAMessage : NSObject
@property(nonatomic, strong) NSData *metadata;
@property(nonatomic, strong) NSArray<NSFileHandle *> *files;
@property(nonatomic, strong) NSArray<IOSurface *> *surfaces;
@property(nonatomic, strong) MTLSharedEventHandle *event;
@end
@implementation JSAMessage
@end

void *jsa_message_new(const uint8_t *bytes, size_t length, const int32_t *fds, size_t fdCount,
                      void *const *surfaces, size_t surfaceCount, void *event) {
  @autoreleasepool {
  JSAMessage *message = [JSAMessage new];
  message.metadata = [NSData dataWithBytes:bytes length:length];
  NSMutableArray *files = [NSMutableArray arrayWithCapacity:fdCount];
  for (size_t i = 0; i < fdCount; i++) {
    int copied = dup(fds[i]);
    if (copied < 0) return NULL;
    [files addObject:[[NSFileHandle alloc] initWithFileDescriptor:copied closeOnDealloc:YES]];
  }
  message.files = files;
  NSMutableArray *objects = [NSMutableArray arrayWithCapacity:surfaceCount];
  for (size_t i = 0; i < surfaceCount; i++) [objects addObject:(__bridge IOSurface *)surfaces[i]];
  message.surfaces = objects;
  message.event = (__bridge MTLSharedEventHandle *)event;
  return (__bridge_retained void *)message;
  }
}

void jsa_object_release(void *object) { (void)(__bridge_transfer id)object; }
void *jsa_object_retain(void *object) { return (__bridge_retained void *)(__bridge id)object; }
const uint8_t *jsa_message_bytes(void *raw, size_t *length) {
  JSAMessage *message = (__bridge JSAMessage *)raw;
  *length = message.metadata.length;
  return message.metadata.bytes;
}
size_t jsa_message_fd_count(void *raw) { return ((__bridge JSAMessage *)raw).files.count; }
size_t jsa_message_surface_count(void *raw) { return ((__bridge JSAMessage *)raw).surfaces.count; }
int32_t jsa_message_copy_fd(void *raw, size_t index) {
  return dup(((__bridge JSAMessage *)raw).files[index].fileDescriptor);
}
void *jsa_message_copy_surface(void *raw, size_t index) {
  return (__bridge_retained void *)((__bridge JSAMessage *)raw).surfaces[index];
}
void *jsa_message_copy_event(void *raw) {
  return (__bridge_retained void *)((__bridge JSAMessage *)raw).event;
}

@protocol JSAAcquisitionXpc
- (void)request:(NSData *)metadata event:(MTLSharedEventHandle *)event
           reply:(void (^)(NSData *, NSArray *, NSArray *, MTLSharedEventHandle *))reply;
@end

static NSXPCInterface *jsa_interface(void) {
  NSXPCInterface *interface = [NSXPCInterface interfaceWithProtocol:@protocol(JSAAcquisitionXpc)];
  SEL request = @selector(request:event:reply:);
  [interface setClasses:[NSSet setWithObject:[MTLSharedEventHandle class]] forSelector:request argumentIndex:1 ofReply:NO];
  [interface setClasses:[NSSet setWithObjects:[NSArray class], [NSFileHandle class], nil]
           forSelector:request argumentIndex:1 ofReply:YES];
  [interface setClasses:[NSSet setWithObjects:[NSArray class], [IOSurface class], nil]
           forSelector:request argumentIndex:2 ofReply:YES];
  [interface setClasses:[NSSet setWithObject:[MTLSharedEventHandle class]] forSelector:request argumentIndex:3 ofReply:YES];
  return interface;
}

typedef void *(*JSARequest)(void *, uint64_t, uint32_t, void *);
typedef void (*JSAClose)(void *, uint64_t);
typedef void (*JSADestroy)(void *);
@class JSADelegate;
@interface JSASession : NSObject <JSAAcquisitionXpc>
@property(nonatomic, strong) JSADelegate *owner;
@property(nonatomic, assign) uint64_t identity;
@property(nonatomic, assign) BOOL closed;
@end
@interface JSADelegate : NSObject <NSXPCListenerDelegate> {
  uint64_t _nextIdentity;
}
@property(nonatomic, assign) void *context;
@property(nonatomic, assign) JSARequest request;
@property(nonatomic, assign) JSAClose close;
@property(nonatomic, assign) JSADestroy destroy;
@property(nonatomic, strong) NSMutableSet<NSXPCConnection *> *connections;
@end
@implementation JSASession
- (void)request:(NSData *)metadata event:(MTLSharedEventHandle *)event
          reply:(void (^)(NSData *, NSArray *, NSArray *, MTLSharedEventHandle *))reply {
  @autoreleasepool {
    @synchronized(self) {
    if (self.closed) { reply(nil, nil, nil, nil); return; }
    JSAMessage *input = [JSAMessage new];
    input.metadata = metadata;
    input.files = @[];
    input.surfaces = @[];
    input.event = event;
    pid_t pid = [NSXPCConnection currentConnection].processIdentifier;
    if (pid <= 0) { reply(nil, nil, nil, nil); return; }
    JSAMessage *output = (__bridge_transfer JSAMessage *)self.owner.request(
        self.owner.context, self.identity, (uint32_t)pid, (__bridge void *)input);
    reply(output.metadata, output.files, output.surfaces, output.event);
    }
  }
}
@end
@implementation JSADelegate
- (instancetype)init {
  if ((self = [super init])) _connections = [NSMutableSet set];
  return self;
}
- (BOOL)listener:(NSXPCListener *)listener shouldAcceptNewConnection:(NSXPCConnection *)connection {
  (void)listener;
  uint64_t identity;
  @synchronized(self) {
    if (_nextIdentity == UINT64_MAX) return NO;
    identity = ++_nextIdentity;
    [self.connections addObject:connection];
  }
  JSASession *session = [JSASession new];
  session.owner = self;
  session.identity = identity;
  connection.exportedInterface = jsa_interface();
  connection.exportedObject = session;
  __weak JSADelegate *weakSelf = self;
  __weak NSXPCConnection *weakConnection = connection;
  __weak JSASession *weakSession = session;
  connection.invalidationHandler = ^{
    JSADelegate *owner = weakSelf;
    if (owner == nil) return;
    @synchronized(owner) {
      NSXPCConnection *ended = weakConnection;
      if (ended != nil) [owner.connections removeObject:ended];
    }
    JSASession *endedSession = weakSession;
    // Serialize invalidation with the full Rust request/reply operation. A
    // request already queued when EOF arrives cannot recreate a removed Rust
    // session and admit an incarnation that never receives its close signal.
    if (endedSession != nil) {
      @synchronized(endedSession) {
        endedSession.closed = YES;
        owner.close(owner.context, identity);
      }
    } else {
      owner.close(owner.context, identity);
    }
  };
  [connection activate];
  return YES;
}
- (void)dealloc { if (_destroy != NULL) _destroy(_context); }
@end

@interface JSAServer : NSObject
@property(nonatomic, strong) NSXPCListener *listener;
@property(nonatomic, strong) JSADelegate *delegate;
@end
@implementation JSAServer
@end
void *jsa_server_start(const char *name, void *context, JSARequest request, JSAClose close, JSADestroy destroy) {
  @autoreleasepool {
  JSADelegate *delegate = [JSADelegate new];
  delegate.context = context;
  delegate.request = request;
  delegate.close = close;
  delegate.destroy = destroy;
  NSXPCListener *listener = name == NULL ? [NSXPCListener anonymousListener] :
      [[NSXPCListener alloc] initWithMachServiceName:[NSString stringWithUTF8String:name]];
  listener.delegate = delegate;
  [listener resume];
  JSAServer *server = [JSAServer new];
  server.listener = listener;
  server.delegate = delegate;
  return (__bridge_retained void *)server;
  }
}
void *jsa_server_endpoint(void *raw) {
  @autoreleasepool {
    return (__bridge_retained void *)((__bridge JSAServer *)raw).listener.endpoint;
  }
}
void jsa_server_stop(void *raw) {
  @autoreleasepool {
  JSAServer *server = (__bridge_transfer JSAServer *)raw;
  [server.listener invalidate];
  NSSet *connections;
  @synchronized(server.delegate) { connections = [server.delegate.connections copy]; }
  for (NSXPCConnection *connection in connections) [connection invalidate];
  }
}

void *jsa_client_connect(const char *name, void *endpoint) {
  @autoreleasepool {
  NSXPCConnection *connection = name == NULL ?
      [[NSXPCConnection alloc] initWithListenerEndpoint:(__bridge NSXPCListenerEndpoint *)endpoint] :
      [[NSXPCConnection alloc] initWithMachServiceName:[NSString stringWithUTF8String:name] options:0];
  connection.remoteObjectInterface = jsa_interface();
  [connection activate];
  return (__bridge_retained void *)connection;
  }
}
void jsa_client_close(void *raw) {
  NSXPCConnection *connection = (__bridge_transfer NSXPCConnection *)raw;
  [connection invalidate];
}
char *jsa_client_request(void *raw, void *inputRaw, void **outReply) {
  @autoreleasepool {
  *outReply = NULL;
  NSXPCConnection *connection = (__bridge NSXPCConnection *)raw;
  JSAMessage *input = (__bridge JSAMessage *)inputRaw;
  __block NSString *failure = nil;
  __block JSAMessage *output = nil;
  id<JSAAcquisitionXpc> proxy = [connection synchronousRemoteObjectProxyWithErrorHandler:^(NSError *error) {
    failure = error.localizedDescription;
  }];
  [proxy request:input.metadata event:input.event
           reply:^(NSData *metadata, NSArray *files, NSArray *surfaces, MTLSharedEventHandle *event) {
    if (metadata == nil) { failure = @"peer returned no acquisition setup reply"; return; }
    output = [JSAMessage new];
    output.metadata = metadata;
    output.files = files;
    output.surfaces = surfaces;
    output.event = event;
  }];
  if (failure != nil || output == nil) {
    return strdup((failure ?: @"acquisition XPC request failed").UTF8String);
  }
  *outReply = (__bridge_retained void *)output;
  return NULL;
  }
}
