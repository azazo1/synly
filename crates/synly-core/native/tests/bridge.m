// 使用假通道验证 run loop 和 socket 桥接, 不扫描或连接任何蓝牙设备.
#import "../bluetooth.m"
#include <assert.h>
#include <poll.h>

@interface FakeDevice : NSObject
@property(nonatomic) BOOL paired;
@property(nonatomic) BluetoothHCIEncryptionMode encryption;
@property(nonatomic) NSUInteger authenticationRequests;
- (BOOL)isPaired;
- (BluetoothHCIEncryptionMode)getEncryptionMode;
- (IOReturn)requestAuthentication;
@end
@implementation FakeDevice
- (BOOL)isPaired { return self.paired; }
- (BluetoothHCIEncryptionMode)getEncryptionMode { return self.encryption; }
- (IOReturn)requestAuthentication { self.authenticationRequests += 1; return kIOReturnSuccess; }
@end

@interface FakeChannel : NSObject
@property(nonatomic, strong) FakeDevice *device;
@property(nonatomic, weak) id target;
@property(nonatomic, strong) NSCondition *progress;
@property(nonatomic, strong) NSMutableData *sent;
@property(nonatomic) BOOL closed;
- (IOBluetoothDevice *)getDevice;
- (BluetoothRFCOMMMTU)getMTU;
- (BOOL)isTransmissionPaused;
- (IOReturn)setDelegate:(id)target;
- (IOReturn)writeAsync:(void *)bytes length:(UInt16)size refcon:(void *)refcon;
- (IOReturn)closeChannel;
@end
@implementation FakeChannel
- (instancetype)init {
    self = [super init];
    if (self) {
        _device = [FakeDevice new];
        _device.paired = YES;
        _device.encryption = (BluetoothHCIEncryptionMode)1;
        _progress = [NSCondition new];
        _sent = [NSMutableData data];
    }
    return self;
}
- (IOBluetoothDevice *)getDevice { return (IOBluetoothDevice *)self.device; }
- (BluetoothRFCOMMMTU)getMTU { return 127; }
- (BOOL)isTransmissionPaused { return NO; }
- (IOReturn)setDelegate:(id)target { _target = target; return kIOReturnSuccess; }
- (IOReturn)writeAsync:(void *)bytes length:(UInt16)size refcon:(void *)refcon {
    (void)refcon;
    assert(size <= [self getMTU]);
    [self.progress lock];
    [self.sent appendBytes:bytes length:size];
    [self.progress broadcast];
    [self.progress unlock];
    [self performSelector:@selector(completeWrite) withObject:nil afterDelay:0];
    return kIOReturnSuccess;
}
- (void)completeWrite {
    [self.target rfcommChannelWriteComplete:(IOBluetoothRFCOMMChannel *)self refcon:NULL status:kIOReturnSuccess];
}
- (IOReturn)closeChannel {
    [self.progress lock];
    self.closed = YES;
    [self.progress broadcast];
    [self.progress unlock];
    return kIOReturnSuccess;
}
@end

static FakeChannel *channel_for(SBConnection *connection) {
    FakeChannel *channel = [FakeChannel new];
    channel.target = connection;
    connection.channel = (IOBluetoothRFCOMMChannel *)channel;
    return channel;
}

static void send_all(int fd, NSData *data) {
    size_t offset = 0;
    NSTimeInterval deadline = NSProcessInfo.processInfo.systemUptime + 3.0;
    while (offset < data.length) {
        assert(NSProcessInfo.processInfo.systemUptime < deadline);
        ssize_t n = send(fd, (const uint8_t *)data.bytes + offset, data.length - offset, 0);
        if (n > 0) offset += (size_t)n;
        else {
            assert(n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR));
            struct pollfd pfd = {fd, POLLOUT, 0};
            assert(poll(&pfd, 1, 1000) > 0);
        }
    }
}

static NSData *receive_all(int fd, size_t size) {
    NSMutableData *data = [NSMutableData dataWithLength:size];
    size_t offset = 0;
    NSTimeInterval deadline = NSProcessInfo.processInfo.systemUptime + 3.0;
    while (offset < size) {
        assert(NSProcessInfo.processInfo.systemUptime < deadline);
        ssize_t n = recv(fd, (uint8_t *)data.mutableBytes + offset, size - offset, 0);
        if (n > 0) offset += (size_t)n;
        else {
            assert(n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR));
            struct pollfd pfd = {fd, POLLIN, 0};
            assert(poll(&pfd, 1, 1000) > 0);
        }
    }
    return data;
}

static void wait_sent(FakeChannel *channel, NSData *expected) {
    NSDate *deadline = [NSDate dateWithTimeIntervalSinceNow:3.0];
    [channel.progress lock];
    while (channel.sent.length < expected.length) assert([channel.progress waitUntilDate:deadline]);
    assert([channel.sent isEqualToData:expected]);
    [channel.progress unlock];
}

static void wait_closed(FakeChannel *channel) {
    NSDate *deadline = [NSDate dateWithTimeIntervalSinceNow:3.0];
    [channel.progress lock];
    while (!channel.closed) assert([channel.progress waitUntilDate:deadline]);
    [channel.progress unlock];
}

static void security_gate(void) {
    run_sync(^{
        SBConnection *connection = [SBConnection new];
        FakeChannel *channel = channel_for(connection);
        channel.device.paired = NO;
        assert([connection bridgeSocket] == -4);
        assert(channel.device.authenticationRequests == 0);
        channel.device.paired = YES;
        channel.device.encryption = kEncryptionDisabled;
        assert([connection bridgeSocket] == -6);
        assert(channel.device.authenticationRequests == 1);
        assert([SBWorker shared].connections.count == 0);
        [connection close];
    });
}

static void transfer_and_cancel(void) {
    __block SBConnection *connection;
    __block FakeChannel *channel;
    __block int fd;
    run_sync(^{
        connection = [SBConnection new];
        channel = channel_for(connection);
        fd = [connection bridgeSocket];
        assert(fd >= 0);
    });
    NSMutableData *payload = [NSMutableData dataWithLength:32768];
    for (NSUInteger i = 0; i < payload.length; ++i) ((uint8_t *)payload.mutableBytes)[i] = (uint8_t)i;
    send_all(fd, payload);
    wait_sent(channel, payload);
    run_sync(^{
        [connection rfcommChannelData:connection.channel data:payload.mutableBytes length:payload.length];
    });
    assert([receive_all(fd, payload.length) isEqualToData:payload]);
    close(fd);
    wait_closed(channel);
    run_sync(^{
        assert(connection.closed);
        assert([SBWorker shared].connections.count == 0);
    });
}

static void receiver_bound(void) {
    __block SBConnection *connection;
    __block FakeChannel *channel;
    __block int fd;
    run_sync(^{
        connection = [SBConnection new];
        channel = channel_for(connection);
        fd = [connection bridgeSocket];
        assert(fd >= 0);
        NSMutableData *oversized = [NSMutableData dataWithLength:kReceiveLimit + 1];
        [connection rfcommChannelData:connection.channel data:oversized.mutableBytes length:oversized.length];
        assert(connection.closed);
        assert([SBWorker shared].connections.count == 0);
    });
    wait_closed(channel);
    uint8_t byte;
    assert(recv(fd, &byte, 1, 0) == 0);
    close(fd);
}

int main(void) {
    @autoreleasepool {
        security_gate();
        transfer_and_cancel();
        receiver_bound();
        fprintf(stdout, "native Bluetooth bridge: 3 tests passed\n");
    }
    return 0;
}
