#pragma once
#include <stdint.h>

// 仅供独立 RTP 音频队列测试包含 Limelight-internal.h.
// 队列不调用 ENet, 这里只提供未使用函数声明需要的不透明类型, 不实现网络替身.
typedef struct _ENetHost ENetHost;
typedef struct _ENetPeer ENetPeer;
typedef struct _ENetEvent ENetEvent;
typedef uint32_t enet_uint32;
