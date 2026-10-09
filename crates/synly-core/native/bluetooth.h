#ifndef SYNLY_BLUETOOTH_H
#define SYNLY_BLUETOOTH_H

#include <stddef.h>
#include <stdint.h>

// 错误码 -1..-10 为桥接层错误, 其余非零值为系统 IOReturn.
typedef struct {
    char address[18];
    char name[256];
} SynlyBluetoothPeer;

typedef void (*SynlyBluetoothAccept)(void *context, int fd, const SynlyBluetoothPeer *peer);

int synly_bt_available(void);
int synly_bt_paired(SynlyBluetoothPeer *peers, size_t capacity, size_t *count);
int synly_bt_query(const char *address, const uint8_t uuid[16], uint8_t *channel);
// 成功后 fd 的所有权交给调用方, 关闭 fd 会取消对应 RFCOMM 通道.
int synly_bt_connect(const char *address, const uint8_t uuid[16], int *fd);
int synly_bt_listen(const uint8_t uuid[16], SynlyBluetoothAccept callback, void *context, void **listener);
// 返回后不会再调用该 listener 的 callback, 可以安全释放 context.
void synly_bt_stop_listener(void *listener);

#endif
