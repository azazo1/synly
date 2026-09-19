#pragma once
#include <cassert>
#include <cstddef>
#include <cstdint>
#include <cstring>

// 仅为编译未经修改的 sdlaud.cpp 提供所需 SDL2 类型与函数声明.
// 这不是产品 SDL bindings, 不加载驱动, 不打开声卡, 延时为虚拟时间.
using Uint8 = std::uint8_t;
using Uint16 = std::uint16_t;
using Uint32 = std::uint32_t;
using SDL_AudioDeviceID = Uint32;
using SDL_AudioFormat = Uint16;
using SDL_AudioCallback = void (*)(void*, Uint8*, int);
struct SDL_AudioSpec {
    int freq;
    SDL_AudioFormat format;
    Uint8 channels;
    Uint8 silence;
    Uint16 samples;
    Uint16 padding;
    Uint32 size;
    SDL_AudioCallback callback;
    void* userdata;
};
enum SDL_AudioStatus { SDL_AUDIO_STOPPED, SDL_AUDIO_PLAYING, SDL_AUDIO_PAUSED };
constexpr Uint32 SDL_INIT_AUDIO = 0x10;
constexpr int SDL_LOG_CATEGORY_APPLICATION = 0;
#if defined(__BYTE_ORDER__) && __BYTE_ORDER__ == __ORDER_BIG_ENDIAN__
constexpr SDL_AudioFormat AUDIO_F32SYS = 0x9120;
#else
constexpr SDL_AudioFormat AUDIO_F32SYS = 0x8120;
#endif
#define SDL_assert(condition) assert(condition)
#define SDL_zero(value) std::memset(&(value), 0, sizeof(value))
#define SDL_max(a, b) ((a) > (b) ? (a) : (b))

Uint32 SDL_WasInit(Uint32 flags);
int SDL_InitSubSystem(Uint32 flags);
void SDL_QuitSubSystem(Uint32 flags);
const char* SDL_GetError();
void SDL_LogError(int category, const char* format, ...);
void SDL_LogInfo(int category, const char* format, ...);
SDL_AudioDeviceID SDL_OpenAudioDevice(const char* device, int capture, const SDL_AudioSpec* want, SDL_AudioSpec* have, int changes);
void SDL_CloseAudioDevice(SDL_AudioDeviceID device);
void SDL_PauseAudioDevice(SDL_AudioDeviceID device, int pause);
void* SDL_malloc(std::size_t bytes);
void SDL_free(void* pointer);
const char* SDL_GetCurrentAudioDriver();
SDL_AudioStatus SDL_GetAudioDeviceStatus(SDL_AudioDeviceID device);
Uint32 SDL_GetQueuedAudioSize(SDL_AudioDeviceID device);
void SDL_Delay(Uint32 milliseconds);
int SDL_QueueAudio(SDL_AudioDeviceID device, const void* data, Uint32 bytes);
