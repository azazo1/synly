// 编译并运行固定上游原始 sdlaud.cpp, 不复制其实现, 不访问真实 SDL/声卡.
#include "sdl.h"
#include <algorithm>
#include <cstdio>
#include <cstdlib>
#include <vector>

namespace {
struct Device {
    bool initialized = false;
    bool opened = false;
    bool fail_open = false;
    bool fail_allocate = false;
    bool fail_queue = false;
    SDL_AudioStatus status = SDL_AUDIO_PLAYING;
    unsigned pending_ms = 0;
    unsigned pending_calls = 0;
    unsigned status_calls = 0;
    unsigned size_calls = 0;
    unsigned delay_calls = 0;
    unsigned virtual_ms = 0;
    unsigned queue_calls = 0;
    unsigned errors = 0;
    unsigned opens = 0;
    unsigned closes = 0;
    unsigned quits = 0;
    unsigned frees = 0;
    unsigned stop_after_delay = 0;
    unsigned drain_after_delay = 0;
    Uint32 queued_bytes = 0;
    Uint32 submitted_bytes = 0;
    std::size_t allocation_size = 0;
    void* allocation = nullptr;
    SDL_AudioSpec requested {};
    std::vector<int> cleanup;
} d;
constexpr SDL_AudioDeviceID device_id = 7;
void reset() {
    assert(!d.initialized && !d.opened && d.allocation == nullptr);
    d = Device {};
}
OPUS_MULTISTREAM_CONFIGURATION config(int frame_ms = 5, int channels = 2) {
    OPUS_MULTISTREAM_CONFIGURATION value {};
    value.sampleRate = 48000;
    value.channelCount = channels;
    value.samplesPerFrame = 48 * frame_ms;
    return value;
}
void prepare(SdlAudioRenderer& renderer, const OPUS_MULTISTREAM_CONFIGURATION& value) {
    assert(renderer.prepareForPlayback(&value));
    assert(renderer.getAudioBufferFormat() == IAudioRenderer::AudioFormat::Float32NE);
    assert(d.allocation_size == static_cast<unsigned>(value.samplesPerFrame * value.channelCount * 4));
    assert(renderer.getAudioBuffer(nullptr) == d.allocation);
    std::memset(d.allocation, 0x5a, d.allocation_size);
}
void check_destroyed(bool had_device, bool had_buffer) {
    assert(!d.initialized && !d.opened && d.allocation == nullptr);
    assert(d.closes == static_cast<unsigned>(had_device));
    assert(d.frees == static_cast<unsigned>(had_buffer));
    assert(d.quits == 1);
    std::vector<int> expected;
    if (had_device) { expected.push_back(1); expected.push_back(2); }
    if (had_buffer) { expected.push_back(3); }
    expected.push_back(4);
    assert(d.cleanup == expected);
}

void format_and_lifecycle() {
    // 上游 want.samples=max(480,3*frame). 不强行等同原生后端的系统设备周期.
    for (int duration : {5, 10, 20, 40, 60}) {
        for (int channels : {2, 6, 8}) {
            reset();
            {
                SdlAudioRenderer renderer;
                const auto value = config(duration, channels);
                prepare(renderer, value);
                assert(d.requested.freq == 48000);
                assert(d.requested.format == AUDIO_F32SYS);
                assert(d.requested.channels == channels);
                assert(d.requested.samples == std::max(480, value.samplesPerFrame * 3));
                assert(d.requested.callback == nullptr && d.requested.userdata == nullptr);
                assert(renderer.submitAudio(static_cast<int>(d.allocation_size)));
                assert(d.submitted_bytes == d.allocation_size);
            }
            check_destroyed(true, true);
        }
    }
    reset();
    {
        SdlAudioRenderer renderer;
        prepare(renderer, config(5));
        assert(renderer.submitAudio(8));
        assert(d.submitted_bytes == 8); // 少于协商帧, 仍为完整 stereo PCM 采样帧.
    }
    check_destroyed(true, true);
    reset();
    {
        SdlAudioRenderer renderer;
        auto short_frame = config();
        short_frame.samplesPerFrame = 120; // 仅测试上游 renderer 的 2.5 ms 下限分支.
        prepare(renderer, short_frame);
        assert(d.requested.samples == 480);
    }
    check_destroyed(true, true);
    std::puts("通过: 15 种格式请求, 480 样本下限, 提交字节及关闭/释放顺序");
}

void pending_network_boundary() {
    for (unsigned pending : {0u, 30u, 31u, 1000u}) {
        reset();
        {
            SdlAudioRenderer renderer;
            prepare(renderer, config());
            d.pending_ms = pending;
            assert(renderer.submitAudio(0));
            assert(d.pending_calls == 0 && d.status_calls == 0 && d.queue_calls == 0);
            assert(renderer.submitAudio(static_cast<int>(d.allocation_size)));
            assert(d.pending_calls == 1);
            assert(d.queue_calls == static_cast<unsigned>(pending <= 30));
            assert(d.status_calls == static_cast<unsigned>(pending <= 30));
        }
        check_destroyed(true, true);
    }
    reset();
    {
        SdlAudioRenderer renderer;
        prepare(renderer, config());
        d.status = SDL_AUDIO_STOPPED;
        d.pending_ms = 31;
        assert(renderer.submitAudio(static_cast<int>(d.allocation_size)));
        assert(d.status_calls == 0); // 积压先丢弃, 设备失效检查会推迟到后续提交.
    }
    check_destroyed(true, true);
    std::puts("通过: 零提交, 网络 30 ms 边界及积压时设备检查顺序");
}

void quantized_queue_boundary() {
    for (int duration : {5, 10, 20, 40, 60}) {
        for (int channels : {2, 6, 8}) {
            const auto value = config(duration, channels);
            const unsigned frame_bytes = value.samplesPerFrame * channels * 4;
            const unsigned next_block = 50 / duration + 1;
            // SDL 原式按完整 packet 向下取整. 比较阈值前一个 PCM 采样帧与阈值本身.
            for (unsigned initial : {next_block * frame_bytes - static_cast<unsigned>(channels * 4), next_block * frame_bytes}) {
                reset();
                {
                    SdlAudioRenderer renderer;
                    prepare(renderer, value);
                    d.queued_bytes = initial;
                    d.drain_after_delay = 3;
                    assert(renderer.submitAudio(static_cast<int>(frame_bytes)));
                    const bool must_wait = initial >= next_block * frame_bytes;
                    assert(d.delay_calls == (must_wait ? 3u : 0u));
                    assert(d.status_calls == (must_wait ? 4u : 1u));
                    assert(d.size_calls == d.status_calls);
                    assert(d.queue_calls == 1);
                }
                check_destroyed(true, true);
            }
        }
    }
    std::puts("通过: 30 种整帧取整水位边界和虚拟消费唤醒");
}

void exhausted_wait_still_queues() {
    reset();
    {
        SdlAudioRenderer renderer;
        prepare(renderer, config());
        d.queued_bytes = static_cast<Uint32>(d.allocation_size * 100);
        assert(renderer.submitAudio(static_cast<int>(d.allocation_size)));
        assert(d.delay_calls == 100 && d.virtual_ms == 100);
        assert(d.status_calls == 100 && d.size_calls == 100);
        assert(d.queue_calls == 1); // 上游100轮后继续排队, 并不返回设备失败.
    }
    check_destroyed(true, true);
    std::puts("通过: 100 次等待耗尽后原始 renderer 仍然入队");
}

void stopped_and_paused_devices() {
    for (unsigned stop_after : {0u, 2u, 100u}) {
        reset();
        {
            SdlAudioRenderer renderer;
            prepare(renderer, config());
            d.queued_bytes = static_cast<Uint32>(d.allocation_size * 100);
            d.stop_after_delay = stop_after;
            if (stop_after == 0) { d.status = SDL_AUDIO_STOPPED; }
            const bool submitted = renderer.submitAudio(static_cast<int>(d.allocation_size));
            // 第100次 delay 后不再检查设备状态, 直接调用 QueueAudio.
            assert(submitted == (stop_after == 100));
            assert(d.queue_calls == static_cast<unsigned>(stop_after == 100));
            assert(d.delay_calls == stop_after);
        }
        check_destroyed(true, true);
    }
    reset();
    {
        SdlAudioRenderer renderer;
        prepare(renderer, config());
        d.status = SDL_AUDIO_PAUSED;
        assert(renderer.submitAudio(static_cast<int>(d.allocation_size)));
        assert(d.queue_calls == 1);
    }
    check_destroyed(true, true);
    std::puts("通过: STOPPED/PAUSED, 等待中失效及最后一轮状态检查边界");
}

void failures_and_cleanup() {
    reset();
    d.fail_open = true;
    {
        SdlAudioRenderer renderer;
        auto value = config();
        assert(!renderer.prepareForPlayback(&value));
        assert(d.errors == 1 && d.allocation == nullptr);
    }
    check_destroyed(false, false);
    reset();
    d.fail_allocate = true;
    {
        SdlAudioRenderer renderer;
        auto value = config();
        assert(!renderer.prepareForPlayback(&value));
        assert(d.errors == 1 && d.opened);
    }
    check_destroyed(true, false);
    reset();
    {
        SdlAudioRenderer renderer;
        prepare(renderer, config());
        d.fail_queue = true;
        assert(renderer.submitAudio(static_cast<int>(d.allocation_size)));
        assert(d.errors == 1 && d.queue_calls == 1); // 原始错误只记录, 返回true.
    }
    check_destroyed(true, true);
    std::puts("通过: 打开/分配失败清理和 QueueAudio 错误返回语义");
}
} // namespace

Uint32 SDL_WasInit(Uint32 flags) { assert(flags == SDL_INIT_AUDIO); return d.initialized ? flags : 0; }
int SDL_InitSubSystem(Uint32 flags) { assert(flags == SDL_INIT_AUDIO && !d.initialized); d.initialized = true; return 0; }
void SDL_QuitSubSystem(Uint32 flags) { assert(flags == SDL_INIT_AUDIO && d.initialized); d.initialized = false; ++d.quits; d.cleanup.push_back(4); }
const char* SDL_GetError() { return "测试设备错误"; }
void SDL_LogError(int, const char*, ...) { ++d.errors; }
void SDL_LogInfo(int, const char*, ...) {}
SDL_AudioDeviceID SDL_OpenAudioDevice(const char* device, int capture, const SDL_AudioSpec* want, SDL_AudioSpec* have, int changes) {
    assert(d.initialized && !d.opened && device == nullptr && capture == 0 && changes == 0);
    ++d.opens;
    d.requested = *want;
    if (d.fail_open) { return 0; }
    *have = *want;
    have->samples = 1024; // 实际系统缓冲允许不同于请求的帧数.
    have->size = have->samples * have->channels * 4;
    d.opened = true;
    return device_id;
}
void SDL_CloseAudioDevice(SDL_AudioDeviceID device) { assert(device == device_id && d.opened); d.opened = false; ++d.closes; d.cleanup.push_back(2); }
void SDL_PauseAudioDevice(SDL_AudioDeviceID device, int pause) {
    assert(device == device_id && d.opened);
    if (pause) { d.cleanup.push_back(1); }
    else { assert(d.allocation != nullptr); }
}
void* SDL_malloc(std::size_t bytes) {
    assert(d.allocation == nullptr);
    if (d.fail_allocate) { return nullptr; }
    d.allocation = std::malloc(bytes);
    assert(d.allocation != nullptr);
    d.allocation_size = bytes;
    return d.allocation;
}
void SDL_free(void* pointer) {
    assert(pointer == d.allocation && pointer != nullptr && !d.opened);
    std::free(pointer); d.allocation = nullptr; ++d.frees; d.cleanup.push_back(3);
}
const char* SDL_GetCurrentAudioDriver() { return "内存替身"; }
SDL_AudioStatus SDL_GetAudioDeviceStatus(SDL_AudioDeviceID device) { assert(device == device_id && d.opened); ++d.status_calls; return d.status; }
Uint32 SDL_GetQueuedAudioSize(SDL_AudioDeviceID device) { assert(device == device_id && d.opened); ++d.size_calls; return d.queued_bytes; }
void SDL_Delay(Uint32 milliseconds) {
    assert(milliseconds == 1); ++d.delay_calls; d.virtual_ms += milliseconds;
    if (d.drain_after_delay && d.delay_calls >= d.drain_after_delay) { d.queued_bytes = 0; }
    if (d.stop_after_delay && d.delay_calls >= d.stop_after_delay) { d.status = SDL_AUDIO_STOPPED; }
}
int SDL_QueueAudio(SDL_AudioDeviceID device, const void* data, Uint32 bytes) {
    assert(device == device_id && d.opened && data == d.allocation && bytes <= d.allocation_size);
    const auto* samples = static_cast<const unsigned char*>(data);
    assert(std::all_of(samples, samples + bytes, [](unsigned char byte) { return byte == 0x5a; }));
    ++d.queue_calls; d.submitted_bytes = bytes;
    if (d.fail_queue) { return -1; }
    d.queued_bytes += bytes;
    return 0;
}
unsigned int LiGetPendingAudioDuration() { ++d.pending_calls; return d.pending_ms; }

int main() {
    format_and_lifecycle();
    pending_network_boundary();
    quantized_queue_boundary();
    exhausted_wait_still_queues();
    stopped_and_paused_devices();
    failures_and_cleanup();
    std::puts("原始 Moonlight SDL renderer 对照完成: 6 组, 无真实设备操作");
}
