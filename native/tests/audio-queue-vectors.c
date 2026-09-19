#include "Limelight-internal.h"
#include <rs.h>
#include <assert.h>
#include <stddef.h>

// 编译原始 RtpAudioQueue.c, 不复制其队列实现. 来源见 docs/audio-queue-vectors.md.
int AppVersionQuad[4] = {7, 1, 415, -1};
int AudioPacketDuration = 5;
CONNECTION_LISTENER_CALLBACKS ListenerCallbacks = {0};
uint64_t PltGetMicroseconds(void) { return 1000000; }

enum { PAYLOAD_SIZE = 16, MAX_EVENTS = 32 };
static unsigned traces;
static reed_solomon *encoder;

typedef struct { uint16_t sequence; int fec_index; } Event;
#define A(seq) {seq, -1}
#define F(seq, idx) {seq, idx}

static void payload(uint16_t sequence, uint8_t *out) {
    for (int i = 0; i < PAYLOAD_SIZE; i++) out[i] = (uint8_t)(sequence * 37 + (sequence >> 8) + i * 17);
}

static void emit_payload(const uint8_t *data, unsigned size) {
    for (unsigned i = 0; i < size; i++) printf("%02x", data[i]);
    putchar(',');
}

static uint16_t make_packet(Event event, uint8_t *bytes) {
    RTP_PACKET *packet = (RTP_PACKET *)bytes;
    *packet = (RTP_PACKET){.header = 0x80, .packetType = event.fec_index < 0 ? 97 : 127,
        .sequenceNumber = event.sequence, .timestamp = (uint32_t)event.sequence * 5, .ssrc = 7};
    if (event.fec_index < 0) {
        payload(event.sequence, bytes + sizeof(*packet));
        return sizeof(*packet) + PAYLOAD_SIZE;
    }
    AUDIO_FEC_HEADER *fec = (AUDIO_FEC_HEADER *)(packet + 1);
    *fec = (AUDIO_FEC_HEADER){.fecShardIndex = (uint8_t)event.fec_index, .payloadType = 97,
        .baseSequenceNumber = htons(event.sequence), .baseTimestamp = htonl((uint32_t)event.sequence * 5), .ssrc = htonl(7)};
    uint8_t *shards[6];
    for (int i = 0; i < 6; i++) {
        shards[i] = reed_solomon_aligned_alloc(PAYLOAD_SIZE);
        assert(shards[i] != NULL);
        if (i < 4) payload((uint16_t)(event.sequence + i), shards[i]);
    }
    assert(reed_solomon_encode(encoder, shards, 6, PAYLOAD_SIZE) == 0);
    memcpy(fec + 1, shards[4 + event.fec_index], PAYLOAD_SIZE);
    for (int i = 0; i < 6; i++) reed_solomon_free(shards[i]);
    return sizeof(*packet) + sizeof(*fec) + PAYLOAD_SIZE;
}

static void run_trace(const char *name, const Event *events, size_t count) {
    RTP_AUDIO_QUEUE queue;
    RtpaInitializeQueue(&queue);
    printf("%s\t", name);
    for (size_t i = 0; i < count; i++) {
        if (i != 0) putchar(' ');
        if (events[i].fec_index < 0) printf("a%u", events[i].sequence);
        else printf("f%u/%d", events[i].sequence, events[i].fec_index);
    }
    putchar('\t');
    for (size_t i = 0; i < count; i++) {
        union { max_align_t alignment; uint8_t bytes[sizeof(RTP_PACKET) + sizeof(AUDIO_FEC_HEADER) + PAYLOAD_SIZE]; } buffer;
        uint16_t length = make_packet(events[i], buffer.bytes);
        int flags = RtpaAddPacket(&queue, (RTP_PACKET *)buffer.bytes, length);
        assert(!RTPQ_PACKET_CONSUMED(flags));
        if (RTPQ_HANDLE_NOW(flags)) emit_payload(buffer.bytes + sizeof(RTP_PACKET), PAYLOAD_SIZE);
        // 归一化队列 API: 每次输入后取尽可用包. 不复现 AudioStream.c 的 HANDLE_NOW 分支调度.
        for (;;) {
            uint16_t returned_size = 0;
            // 非零自定义头避免 missing 占位依赖 malloc(0) 的平台行为.
            RTP_PACKET *storage = RtpaGetQueuedPacket(&queue, 8, &returned_size);
            if (storage == NULL) break;
            if (returned_size == 0) printf("missing,");
            else {
                assert(returned_size == sizeof(RTP_PACKET) + PAYLOAD_SIZE);
                emit_payload((uint8_t *)storage + 8 + sizeof(RTP_PACKET), PAYLOAD_SIZE);
            }
            free(storage);
        }
        putchar(';');
    }
    putchar('\n');
    RtpaCleanupQueue(&queue);
    traces++;
}

static void permutations(Event *events, int offset, unsigned loss_mask) {
    if (offset == 5) {
        char name[80];
        snprintf(name, sizeof(name), "loss-%02x-order-%u%u%u%u", loss_mask,
            events[1].fec_index < 0 ? events[1].sequence - 4 : 4 + events[1].fec_index,
            events[2].fec_index < 0 ? events[2].sequence - 4 : 4 + events[2].fec_index,
            events[3].fec_index < 0 ? events[3].sequence - 4 : 4 + events[3].fec_index,
            events[4].fec_index < 0 ? events[4].sequence - 4 : 4 + events[4].fec_index);
        run_trace(name, events, 5);
        return;
    }
    for (int i = offset; i < 5; i++) {
        Event saved = events[i]; events[i] = events[offset]; events[offset] = saved;
        permutations(events, offset + 1, loss_mask);
        saved = events[i]; events[i] = events[offset]; events[offset] = saved;
    }
}

int main(void) {
    reed_solomon_init();
    encoder = reed_solomon_new(4, 2);
    assert(encoder != NULL);
    const uint8_t matrix[] = {0x77, 0x40, 0x38, 0x0e, 0xc7, 0xa7, 0x0d, 0x6c};
    memcpy(encoder->p, matrix, sizeof(matrix));
    fprintf(stderr, "生成原始 Moonlight 队列轨迹: 15 种双丢片组合, 每组 24 种到达顺序\n");
    for (int first = 0; first < 6; first++) {
        for (int second = first + 1; second < 6; second++) {
            Event events[5] = {A(0)};
            int next = 1;
            for (int shard = 0; shard < 6; shard++) {
                if (shard != first && shard != second) events[next++] = (Event){shard < 4 ? (uint16_t)(4 + shard) : 4, shard < 4 ? -1 : shard - 4};
            }
            permutations(events, 1, (1u << first) | (1u << second));
        }
    }
    const struct { const char *name; size_t count; Event events[MAX_EVENTS]; } cases[] = {
        {"ordered", 9, {A(0), A(4), A(5), A(6), A(7), A(8), A(9), A(10), A(11)}},
        {"duplicates", 9, {A(0), A(6), A(6), F(4, 0), F(4, 0), A(4), A(7), A(5), A(5)}},
        {"parity-first", 7, {A(0), F(4, 1), F(4, 0), A(7), A(6), A(4), A(5)}},
        {"whole-block-gap", 6, {A(0), A(12), A(13), A(14), A(15), A(16)}},
        {"partial-loss", 8, {A(0), A(4), A(7), A(8), A(9), A(10), A(11), A(12)}},
        {"late-enables-wait", 10, {A(0), A(4), A(5), A(6), A(7), A(4), A(8), A(11), A(12), A(9)}},
        {"mixed-payloads", 8, {A(0), A(4), A(6), F(4, 1), A(8), F(4, 0), A(9), A(10)}},
        {"startup-parity", 6, {F(0, 1), A(2), A(3), A(4), A(5), A(6)}},
        {"near-wrap", 9, {A(65524), A(65528), A(65529), A(65530), A(65531), A(65532), A(65533), A(65534), A(65535)}},
        {"startup-wrap-difference", 6, {A(65532), A(0), A(1), A(2), A(3), A(4)}},
    };
    for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) run_trace(cases[i].name, cases[i].events, cases[i].count);
    reed_solomon_release(encoder);
    fprintf(stderr, "完成 %u 条真实队列轨迹\n", traces);
    return 0;
}
