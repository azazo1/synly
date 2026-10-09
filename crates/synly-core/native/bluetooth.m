// 阻塞等待和字节流桥接由专用 run loop 承担; SDP 启动由主线程处理, 回调跨线程发布.
#import "bluetooth.h"
#import <Foundation/Foundation.h>
#import <CoreBluetooth/CoreBluetooth.h>
#import <IOBluetooth/IOBluetooth.h>
#include <errno.h>
#include <fcntl.h>
#include <sys/socket.h>
#include <unistd.h>

static const NSUInteger kReceiveLimit = 64 * 1024;

@interface SBRpc : NSObject
@property(nonatomic, copy) void (^work)(void);
@property(atomic) BOOL complete;
@end
@implementation SBRpc
@end

@class SBConnection;
@class SBQuery;
@interface SBWorker : NSObject
@property(nonatomic, strong) NSThread *thread;
@property(nonatomic, strong) NSCondition *ready;
@property(nonatomic) BOOL running;
@property(nonatomic, strong) NSMutableDictionary<NSNumber *, SBConnection *> *connections;
@property(nonatomic, strong) NSMutableArray<SBQuery *> *queries;
@property(nonatomic) BOOL listening;
+ (instancetype)shared;
- (void)execute:(SBRpc *)rpc;
@end

static int availability(void) {
    if (@available(macOS 10.15, *)) {
        CBManagerAuthorization authorization = [CBManager authorization];
        if (authorization == CBManagerAuthorizationDenied || authorization == CBManagerAuthorizationRestricted) return -2;
    }
    IOBluetoothHostController *controller = [IOBluetoothHostController defaultController];
    if (controller == nil) return -3;
    if (controller.powerState != kBluetoothHCIPowerStateON) return -1;
    return 0;
}

static void peer_info(IOBluetoothDevice *device, SynlyBluetoothPeer *peer) {
    memset(peer, 0, sizeof(*peer));
    snprintf(peer->address, sizeof(peer->address), "%s", device.addressString.UTF8String ?: "");
    snprintf(peer->name, sizeof(peer->name), "%s", device.nameOrAddress.UTF8String ?: "");
}

static IOBluetoothDevice *paired_device(const char *address) {
    NSString *value = [[NSString stringWithUTF8String:address] stringByReplacingOccurrencesOfString:@":" withString:@"-"];
    IOBluetoothDevice *device = [IOBluetoothDevice deviceWithAddressString:value];
    return device.isPaired ? device : nil;
}

@interface SBQuery : NSObject
@property(nonatomic, strong) IOBluetoothDevice *device;
// 系统可能从主线程投递回调, 完成标志必须原子发布, 不能依赖 worker 的 run loop 隐含同步.
@property(atomic) BOOL complete;
@property(atomic) IOReturn status;
@property(nonatomic) BOOL connectionStarted;
@property(atomic) BOOL connectionComplete;
@property(atomic) IOReturn connectionStatus;
@property(nonatomic) BOOL sdpStarted;
@property(atomic) BOOL abandoned;
@property(atomic) BOOL requestReturned;
@property(atomic) IOReturn requestStatus;
@property(atomic) BOOL requestOnMain;
@property(atomic) BOOL callbackReceived;
@property(atomic) BOOL callbackOnMain;
@property(atomic) BOOL callbackMatchesDevice;
- (void)connectionComplete:(IOBluetoothDevice *)device status:(IOReturn)status;
- (void)sdpQueryComplete:(IOBluetoothDevice *)device status:(IOReturn)status;
@end
@implementation SBQuery
- (void)connectionComplete:(IOBluetoothDevice *)device status:(IOReturn)status {
    if (device != self.device) return;
    self.connectionStatus = status;
    self.connectionComplete = YES;
}
- (void)sdpQueryComplete:(IOBluetoothDevice *)device status:(IOReturn)status {
    self.callbackOnMain = NSThread.isMainThread;
    self.callbackMatchesDevice = device == self.device;
    self.callbackReceived = YES;
    if (!self.callbackMatchesDevice) return;
    self.status = status;
    self.complete = YES;
}
@end

static void retire_query(SBWorker *worker, SBQuery *query) {
    query.abandoned = YES;
    // ACL 已连接但回调缺失时仍可能迟到, 不能因 SDP 已完成就释放 callback target.
    if ((!query.connectionStarted || query.connectionComplete) && (!query.sdpStarted || query.complete)) {
        [worker.queries removeObject:query];
    }
}

static void pump_loop(void) {
    [[NSRunLoop currentRunLoop] runMode:NSDefaultRunLoopMode beforeDate:[NSDate dateWithTimeIntervalSinceNow:0.01]];
}

static int query_channel_run(IOBluetoothDevice *device, const uint8_t uuid[16], uint8_t *channel, uint8_t *stage, SBWorker *worker, SBQuery *query) {
    IOBluetoothSDPUUID *service = [IOBluetoothSDPUUID uuidWithBytes:uuid length:16];
    // Monterey 及之后的 UUID 过滤查询可能成功返回却不执行, 且 SDP 不再自动建立 ACL.
    // 先显式连接已配对设备, 再查询全部 SDP 记录, 结果仍严格按 Synly UUID 选择.
    *stage = 7;
    if (!device.isConnected) {
        // SDP 仅作发现, 不要求链路认证以免扫描触发重新配对; RFCOMM 在 bridgeSocket 独立强制认证/加密.
        IOReturn status = [device openConnection:query withPageTimeout:(BluetoothHCIPageTimeout)0x2000 authenticationRequired:NO];
        query.connectionStarted = status == kIOReturnSuccess;
        if (status != kIOReturnSuccess && status != kIOBluetoothConnectionAlreadyExists) {
            retire_query(worker, query);
            return (int)status;
        }
        NSTimeInterval deadline = NSProcessInfo.processInfo.systemUptime + 20.0;
        // 部分系统可能已连接但不发送 connectionComplete, 同时观察实际连接状态.
        // 成功回调本身就是 ACL 已建立的确认, 不应继续等待 isConnected 属性刷新.
        // 只有没有回调时才依赖设备状态作为备用成功信号.
        while (!device.isConnected && !query.connectionComplete && NSProcessInfo.processInfo.systemUptime < deadline) pump_loop();
        if (!device.isConnected && !query.connectionComplete) {
            query.abandoned = YES;
            return -5;
        }
        if (query.connectionComplete && query.connectionStatus != kIOReturnSuccess) {
            retire_query(worker, query);
            return (int)query.connectionStatus;
        }
    }
    if (!device.isPaired) { retire_query(worker, query); return -4; }
    *stage = 4;
    query.sdpStarted = YES;
    NSTimeInterval deadline = NSProcessInfo.processInfo.systemUptime + 10.0;
    // 实机对照确认 worker 发起 SDP 可成功, 完成回调仍须由主线程事件循环处理.
    query.requestOnMain = NSThread.isMainThread;
    query.requestStatus = [device performSDPQuery:query];
    if (query.requestStatus != kIOReturnSuccess) {
        query.status = query.requestStatus;
        query.complete = YES;
    }
    query.requestReturned = YES;
    if (query.requestStatus != kIOReturnSuccess) {
        query.sdpStarted = NO;
        retire_query(worker, query);
        return (int)query.requestStatus;
    }
    *stage = 5;
    while (!query.complete && NSProcessInfo.processInfo.systemUptime < deadline) pump_loop();
    // 无 API 可取消 SDP; 保留回调对象和设备到系统完成, 不关闭可能被其它路径使用的 ACL.
    if (!query.complete) { query.abandoned = YES; return -5; }
    retire_query(worker, query);
    if (query.status != kIOReturnSuccess) return (int)query.status;
    *stage = 6;
    IOBluetoothSDPServiceRecord *record = [device getServiceRecordForUUID:service];
    if (record == nil) return 0;
    BluetoothRFCOMMChannelID found = 0;
    IOReturn status = [record getRFCOMMChannelID:&found];
    if (status != kIOReturnSuccess) return (int)status;
    if (found < 1 || found > 30) return -7;
    *channel = found;
    return 0;
}

static int query_channel_traced(IOBluetoothDevice *device, const uint8_t uuid[16], uint8_t *channel, uint8_t *stage, SynlyBluetoothQueryTrace *trace) {
    if (trace != NULL) memset(trace, 0, sizeof(*trace));
    *channel = 0;
    *stage = 2;
    if (!device.isPaired) return -4;
    *stage = 3;
    SBWorker *worker = [SBWorker shared];
    if (worker.queries.count >= 32) return -9;
    SBQuery *query = [SBQuery new];
    query.device = device;
    [worker.queries addObject:query];
    int result = query_channel_run(device, uuid, channel, stage, worker, query);
    if (trace != NULL) {
        trace->request_returned = query.requestReturned;
        trace->request_on_main = query.requestOnMain;
        trace->callback_received = query.callbackReceived;
        trace->callback_on_main = query.callbackOnMain;
        trace->callback_matches_device = query.callbackMatchesDevice;
    }
    return result;
}

static int query_channel(IOBluetoothDevice *device, const uint8_t uuid[16], uint8_t *channel, uint8_t *stage) {
    return query_channel_traced(device, uuid, channel, stage, NULL);
}

@interface SBConnection : NSObject <IOBluetoothRFCOMMChannelDelegate>
@property(nonatomic, strong) IOBluetoothRFCOMMChannel *channel;
@property(nonatomic) CFSocketRef socket;
@property(nonatomic) int fd;
@property(nonatomic) BOOL closed;
@property(nonatomic) BOOL opened;
@property(nonatomic) IOReturn openStatus;
@property(nonatomic, strong) NSMutableData *pendingReceive;
@property(nonatomic, strong) NSData *pendingWrite;
@property(nonatomic) NSTimeInterval writeStarted;
- (int)bridgeSocket;
- (void)pumpOutbound;
- (void)pumpInbound;
- (void)close;
@end

static const void *retain_connection(const void *info) { return CFRetain(info); }
static void release_connection(const void *info) { CFRelease(info); }
static void socket_event(CFSocketRef socket, CFSocketCallBackType kind, CFDataRef address, const void *data, void *info) {
    (void)socket; (void)address; (void)data;
    SBConnection *connection = (__bridge SBConnection *)info;
    if (kind == kCFSocketReadCallBack) [connection pumpOutbound];
    if (kind == kCFSocketWriteCallBack) [connection pumpInbound];
}

@implementation SBConnection
- (instancetype)init {
    self = [super init];
    if (self) {
        _fd = -1;
        _pendingReceive = [NSMutableData data];
    }
    return self;
}
- (int)bridgeSocket {
    IOBluetoothDevice *device = [self.channel getDevice];
    if (!device.isPaired) return -4;
    // 只对已有系统配对请求链路认证, 不为未配对设备启动配对流程.
    if ([device getEncryptionMode] == kEncryptionDisabled) {
        IOReturn status = [device requestAuthentication];
        if (status != kIOReturnSuccess) return (int)status;
    }
    if (!device.isPaired || [device getEncryptionMode] == kEncryptionDisabled) return -6;
    if (self.closed || self.channel == nil) return -7;
    int pair[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair) != 0) return -7;
    int noSigpipe = 1;
    int socketBuffer = 2048;
    for (int i = 0; i < 2; ++i) {
        int flags = fcntl(pair[i], F_GETFL, 0);
        if (flags < 0 || fcntl(pair[i], F_SETFL, flags | O_NONBLOCK) != 0 ||
            setsockopt(pair[i], SOL_SOCKET, SO_NOSIGPIPE, &noSigpipe, sizeof(noSigpipe)) != 0 ||
            setsockopt(pair[i], SOL_SOCKET, SO_SNDBUF, &socketBuffer, sizeof(socketBuffer)) != 0 ||
            setsockopt(pair[i], SOL_SOCKET, SO_RCVBUF, &socketBuffer, sizeof(socketBuffer)) != 0) {
            close(pair[0]); close(pair[1]); return -7;
        }
    }
    self.fd = pair[1];
    CFSocketContext context = {0, (__bridge void *)self, retain_connection, release_connection, NULL};
    self.socket = CFSocketCreateWithNative(NULL, self.fd, kCFSocketReadCallBack | kCFSocketWriteCallBack, socket_event, &context);
    if (self.socket == NULL) {
        close(pair[0]); close(pair[1]); self.fd = -1; return -7;
    }
    CFOptionFlags flags = CFSocketGetSocketFlags(self.socket);
    flags &= ~(kCFSocketAutomaticallyReenableReadCallBack | kCFSocketAutomaticallyReenableWriteCallBack);
    CFSocketSetSocketFlags(self.socket, flags | kCFSocketCloseOnInvalidate);
    CFSocketDisableCallBacks(self.socket, kCFSocketWriteCallBack);
    CFRunLoopSourceRef source = CFSocketCreateRunLoopSource(NULL, self.socket, 0);
    if (source == NULL) {
        close(pair[0]); [self close]; return -7;
    }
    CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
    CFRelease(source);
    [SBWorker shared].connections[@(self.fd)] = self;
    [self pumpInbound];
    return pair[0];
}
- (void)pumpOutbound {
    if (self.closed || self.socket == NULL) return;
    if (self.pendingWrite != nil || self.channel.isTransmissionPaused) {
        CFSocketDisableCallBacks(self.socket, kCFSocketReadCallBack);
        return;
    }
    uint8_t bytes[1024];
    size_t mtu = MIN(sizeof(bytes), (size_t)[self.channel getMTU]);
    if (mtu == 0) { [self close]; return; }
    ssize_t size = recv(self.fd, bytes, mtu, 0);
    if (size == 0) { [self close]; return; }
    if (size < 0) {
        if (errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR) { [self close]; return; }
        CFSocketEnableCallBacks(self.socket, kCFSocketReadCallBack);
        return;
    }
    self.pendingWrite = [NSData dataWithBytes:bytes length:(NSUInteger)size];
    self.writeStarted = NSProcessInfo.processInfo.systemUptime;
    CFSocketDisableCallBacks(self.socket, kCFSocketReadCallBack);
    IOReturn status = [self.channel writeAsync:(void *)self.pendingWrite.bytes length:(UInt16)size refcon:NULL];
    if (status != kIOReturnSuccess) [self close];
}
- (void)pumpInbound {
    if (self.closed || self.socket == NULL) return;
    while (self.pendingReceive.length > 0) {
        ssize_t size = send(self.fd, self.pendingReceive.bytes, self.pendingReceive.length, 0);
        if (size > 0) {
            [self.pendingReceive replaceBytesInRange:NSMakeRange(0, (NSUInteger)size) withBytes:NULL length:0];
        } else if (size < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
            CFSocketEnableCallBacks(self.socket, kCFSocketWriteCallBack);
            return;
        } else if (size < 0 && errno == EINTR) {
            continue;
        } else {
            [self close]; return;
        }
    }
    CFSocketDisableCallBacks(self.socket, kCFSocketWriteCallBack);
}
- (void)rfcommChannelData:(IOBluetoothRFCOMMChannel *)channel data:(void *)data length:(size_t)length {
    (void)channel;
    if (self.closed) return;
    if (length > kReceiveLimit - self.pendingReceive.length) { [self close]; return; }
    [self.pendingReceive appendBytes:data length:length];
    [self pumpInbound];
}
- (void)rfcommChannelOpenComplete:(IOBluetoothRFCOMMChannel *)channel status:(IOReturn)status {
    if (self.closed) { [channel closeChannel]; return; }
    self.channel = channel;
    self.openStatus = status;
    self.opened = YES;
}
- (void)rfcommChannelClosed:(IOBluetoothRFCOMMChannel *)channel {
    (void)channel; [self close];
}
- (void)rfcommChannelWriteComplete:(IOBluetoothRFCOMMChannel *)channel refcon:(void *)refcon status:(IOReturn)status {
    (void)channel; (void)refcon;
    if (self.closed) return;
    self.pendingWrite = nil;
    if (status != kIOReturnSuccess) { [self close]; return; }
    [self pumpOutbound];
    if (!self.closed && self.pendingWrite == nil && !self.channel.isTransmissionPaused) {
        CFSocketEnableCallBacks(self.socket, kCFSocketReadCallBack);
    }
}
- (void)rfcommChannelFlowControlChanged:(IOBluetoothRFCOMMChannel *)channel {
    (void)channel;
    if (!self.closed && self.pendingWrite == nil && !self.channel.isTransmissionPaused) [self pumpOutbound];
}
- (void)close {
    if (self.closed) return;
    self.closed = YES;
    [self.channel setDelegate:nil];
    [self.channel closeChannel];
    self.channel = nil;
    self.pendingWrite = nil;
    [self.pendingReceive setLength:0];
    if (self.socket != NULL) {
        CFSocketInvalidate(self.socket);
        CFRelease(self.socket);
        self.socket = NULL;
    }
    if (self.fd >= 0) [[SBWorker shared].connections removeObjectForKey:@(self.fd)];
    self.fd = -1;
}
@end

@implementation SBWorker
+ (instancetype)shared {
    static SBWorker *worker;
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        worker = [SBWorker new];
        worker.ready = [NSCondition new];
        worker.connections = [NSMutableDictionary dictionary];
        worker.queries = [NSMutableArray array];
        worker.thread = [[NSThread alloc] initWithTarget:worker selector:@selector(run) object:nil];
        worker.thread.name = @"Synly Bluetooth";
        [worker.ready lock];
        [worker.thread start];
        while (!worker.running) [worker.ready wait];
        [worker.ready unlock];
    });
    return worker;
}
- (void)run {
    @autoreleasepool {
        [NSTimer scheduledTimerWithTimeInterval:1.0 target:self selector:@selector(checkConnections:) userInfo:nil repeats:YES];
        [self.ready lock];
        self.running = YES;
        [self.ready broadcast];
        [self.ready unlock];
        for (;;) {
            @autoreleasepool {
                [[NSRunLoop currentRunLoop] runMode:NSDefaultRunLoopMode beforeDate:[NSDate distantFuture]];
            }
        }
    }
}
- (void)execute:(SBRpc *)rpc {
    @try { rpc.work(); }
    @finally { rpc.complete = YES; }
}
- (void)checkConnections:(NSTimer *)timer {
    (void)timer;
    for (SBConnection *connection in self.connections.allValues) {
        IOBluetoothDevice *device = [connection.channel getDevice];
        if (!device.isPaired || [device getEncryptionMode] == kEncryptionDisabled ||
            (connection.pendingWrite != nil && NSProcessInfo.processInfo.systemUptime - connection.writeStarted > 10.0)) {
            [connection close];
        }
    }
    NSIndexSet *complete = [self.queries indexesOfObjectsPassingTest:^BOOL(SBQuery *query, NSUInteger index, BOOL *stop) {
        (void)index; (void)stop;
        return query.abandoned && (!query.connectionStarted || query.connectionComplete) && (!query.sdpStarted || query.complete);
    }];
    [self.queries removeObjectsAtIndexes:complete];
}
@end

static void run_sync(void (^work)(void)) {
    SBWorker *worker = [SBWorker shared];
    if ([NSThread currentThread] == worker.thread) { work(); return; }
    SBRpc *rpc = [SBRpc new];
    rpc.work = work;
    BOOL onMain = NSThread.isMainThread;
    [worker performSelector:@selector(execute:) onThread:worker.thread withObject:rpc waitUntilDone:!onMain];
    // 主线程调用 FFI 或原生回归时, 不能用同步等待堵住系统回调的投递线程.
    if (onMain) while (!rpc.complete) pump_loop();
}

@interface SBListener : NSObject
@property(nonatomic, strong) IOBluetoothSDPServiceRecord *record;
@property(nonatomic, strong) IOBluetoothUserNotification *notification;
@property(nonatomic) SynlyBluetoothAccept callback;
@property(nonatomic) void *context;
@property(nonatomic) BOOL stopped;
- (void)opened:(IOBluetoothUserNotification *)notification channel:(IOBluetoothRFCOMMChannel *)channel;
- (void)stop;
@end
@implementation SBListener
- (void)opened:(IOBluetoothUserNotification *)notification channel:(IOBluetoothRFCOMMChannel *)channel {
    (void)notification;
    if (self.stopped || !channel.isIncoming || ![channel getDevice].isPaired) { [channel closeChannel]; return; }
    SBConnection *connection = [SBConnection new];
    connection.channel = channel;
    IOReturn status = [channel setDelegate:connection];
    if (status != kIOReturnSuccess) { [connection close]; return; }
    int fd = [connection bridgeSocket];
    if (fd < 0) { [connection close]; return; }
    // 链路认证可能重入 run loop; 返回时 listener 可能已经被用户停用.
    if (self.stopped || self.callback == NULL) { close(fd); [connection close]; return; }
    SynlyBluetoothPeer peer;
    peer_info([channel getDevice], &peer);
    self.callback(self.context, fd, &peer);
}
- (void)stop {
    if (self.stopped) return;
    self.stopped = YES;
    [self.notification unregister];
    self.notification = nil;
    [self.record removeServiceRecord];
    self.record = nil;
    self.callback = NULL;
    self.context = NULL;
    [SBWorker shared].listening = NO;
}
@end

int synly_bt_available(void) {
    __block int result;
    run_sync(^{ result = availability(); });
    return result;
}
int synly_bt_paired(SynlyBluetoothPeer *peers, size_t capacity, size_t *count) {
    __block int result;
    run_sync(^{
        result = availability();
        if (result != 0) return;
        NSArray<IOBluetoothDevice *> *devices = [IOBluetoothDevice pairedDevices];
        *count = devices.count;
        if (devices.count > capacity) { result = -10; return; }
        NSUInteger index = 0;
        for (IOBluetoothDevice *device in devices) peer_info(device, &peers[index++]);
    });
    return result;
}
int synly_bt_query(const char *address, const uint8_t uuid[16], uint8_t *channel, uint8_t *stage, SynlyBluetoothQueryTrace *trace) {
    if (trace != NULL) memset(trace, 0, sizeof(*trace));
    *channel = 0;
    *stage = 1;
    __block int result;
    run_sync(^{
        result = availability();
        if (result != 0) return;
        *stage = 2;
        IOBluetoothDevice *device = paired_device(address);
        if (device == nil) { result = -4; return; }
        result = query_channel_traced(device, uuid, channel, stage, trace);
    });
    return result;
}
int synly_bt_connect(const char *address, const uint8_t uuid[16], int *fd) {
    *fd = -1;
    __block int result;
    run_sync(^{
        result = availability();
        if (result != 0) return;
        IOBluetoothDevice *device = paired_device(address);
        if (device == nil) { result = -4; return; }
        uint8_t channelID = 0;
        uint8_t stage = 0;
        result = query_channel(device, uuid, &channelID, &stage);
        if (result != 0) return;
        if (channelID == 0) { result = -8; return; }
        SBConnection *connection = [SBConnection new];
        IOBluetoothRFCOMMChannel *channel = nil;
        IOReturn status = [device openRFCOMMChannelAsync:&channel withChannelID:channelID delegate:connection];
        if (status != kIOReturnSuccess) { result = (int)status; [connection close]; return; }
        connection.channel = channel;
        // IOBluetooth 的 out 参数返回已 retain 的 channel, ARC 不知道这项旧 API 约定.
        if (channel != nil) CFRelease((__bridge CFTypeRef)channel);
        NSTimeInterval deadline = NSProcessInfo.processInfo.systemUptime + 15.0;
        while (!connection.opened && !connection.closed && NSProcessInfo.processInfo.systemUptime < deadline) pump_loop();
        if (!connection.opened || connection.closed) { result = -5; [connection close]; return; }
        if (connection.openStatus != kIOReturnSuccess) { result = (int)connection.openStatus; [connection close]; return; }
        int bridge = [connection bridgeSocket];
        if (bridge < 0) { result = bridge; [connection close]; return; }
        *fd = bridge;
        result = 0;
    });
    return result;
}
int synly_bt_listen(const uint8_t uuid[16], SynlyBluetoothAccept callback, void *context, void **listener) {
    *listener = NULL;
    __block int result;
    run_sync(^{
        result = availability();
        if (result != 0) return;
        if ([SBWorker shared].listening) { result = -9; return; }
        IOBluetoothSDPUUID *service = [IOBluetoothSDPUUID uuidWithBytes:uuid length:16];
        NSDictionary *byte = @{@"DataElementType": @1, @"DataElementSize": @1, @"DataElementValue": @1};
        NSDictionary *attributes = @{
            @"0001 - ServiceClassIDList": @[service],
            @"0004 - ProtocolDescriptorList": @[
                @[[IOBluetoothSDPUUID uuid16:kBluetoothSDPUUID16L2CAP]],
                @[[IOBluetoothSDPUUID uuid16:kBluetoothSDPUUID16RFCOMM], byte]
            ],
            @"0005 - BrowseGroupList": @[[IOBluetoothSDPUUID uuid16:kBluetoothSDPUUID16ServiceClassPublicBrowseGroup]],
            @"0100 - ServiceName": @"Synly",
            @"LocalAttributes": @{@"Persistent": @NO}
        };
        SBListener *entry = [SBListener new];
        entry.record = [IOBluetoothSDPServiceRecord publishedServiceRecordWithDictionary:attributes];
        if (entry.record == nil) { result = -7; return; }
        BluetoothRFCOMMChannelID channel = 0;
        IOReturn status = [entry.record getRFCOMMChannelID:&channel];
        if (status != kIOReturnSuccess || channel < 1 || channel > 30) { [entry stop]; result = -7; return; }
        entry.callback = callback;
        entry.context = context;
        entry.notification = [IOBluetoothRFCOMMChannel registerForChannelOpenNotifications:entry selector:@selector(opened:channel:) withChannelID:channel direction:kIOBluetoothUserNotificationChannelDirectionIncoming];
        if (entry.notification == nil) { [entry stop]; result = -7; return; }
        [SBWorker shared].listening = YES;
        *listener = (void *)CFBridgingRetain(entry);
        result = 0;
    });
    return result;
}
void synly_bt_stop_listener(void *listener) {
    if (listener == NULL) return;
    SBListener *entry = CFBridgingRelease(listener);
    run_sync(^{ [entry stop]; });
}
