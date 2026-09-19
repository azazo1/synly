#pragma once

// 仅保留原始 renderer 读取的布局, 不模拟传输或完整 Moonlight 会话.
typedef struct _OPUS_MULTISTREAM_CONFIGURATION {
    int sampleRate;
    int channelCount;
    int streams;
    int coupledStreams;
    int samplesPerFrame;
    unsigned char mapping[8];
} OPUS_MULTISTREAM_CONFIGURATION, *POPUS_MULTISTREAM_CONFIGURATION;

unsigned int LiGetPendingAudioDuration(void);
