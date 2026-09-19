#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <rs.h>

/* 来源与许可见 docs/audio-fec-vectors.md 和 audio-fec-nanors-LICENSE.txt. */
enum { DATA_SHARDS = 4, PARITY_SHARDS = 2, TOTAL_SHARDS = 6 };

static int check_recovery(reed_solomon *rs, uint8_t **original, int size,
                          unsigned mask) {
    uint8_t *shards[TOTAL_SHARDS] = {0};
    uint8_t marks[TOTAL_SHARDS] = {0};
    int result = -1;
    for (int shard = 0; shard < TOTAL_SHARDS; ++shard) {
        shards[shard] = reed_solomon_aligned_alloc((size_t)size);
        if (!shards[shard]) {
            goto done;
        }
        marks[shard] = (uint8_t)((mask >> shard) & 1);
        if (marks[shard]) {
            memset(shards[shard], 0xa5, (size_t)size);
        } else {
            memcpy(shards[shard], original[shard], (size_t)size);
        }
    }
    if (reed_solomon_decode(rs, shards, marks, TOTAL_SHARDS, size) != 0) {
        goto done;
    }
    for (int shard = 0; shard < DATA_SHARDS; ++shard) {
        if (memcmp(shards[shard], original[shard], (size_t)size) != 0) {
            goto done;
        }
    }
    result = 0;
done:
    for (int shard = 0; shard < TOTAL_SHARDS; ++shard) {
        reed_solomon_free(shards[shard]);
    }
    if (result != 0) {
        fprintf(stderr, "上游恢复失败: size=%d mask=0x%x\n", size, mask);
    }
    return result;
}

static int emit_vector(reed_solomon *rs, const char *name, int size, int basis) {
    uint8_t *shards[TOTAL_SHARDS] = {0};
    int result = -1;
    for (int shard = 0; shard < TOTAL_SHARDS; ++shard) {
        shards[shard] = reed_solomon_aligned_alloc((size_t)size);
        if (!shards[shard]) {
            goto done;
        }
        if (shard < DATA_SHARDS) {
            for (int byte = 0; byte < size; ++byte) {
                shards[shard][byte] = basis ? (uint8_t)(shard == byte)
                    : (uint8_t)(byte * 73 + shard * 41);
            }
        }
    }
    if (reed_solomon_encode(rs, shards, TOTAL_SHARDS, size) != 0) {
        goto done;
    }
    /* 检查全部单丢和双丢组合, 其中包括只丢校验片. */
    for (int first = 0; first < TOTAL_SHARDS; ++first) {
        if (check_recovery(rs, shards, size, 1u << first) != 0) {
            goto done;
        }
        for (int second = first + 1; second < TOTAL_SHARDS; ++second) {
            if (check_recovery(rs, shards, size, (1u << first) | (1u << second)) != 0) {
                goto done;
            }
        }
    }
    printf("    (\"%s\", %d, [\n", name, size);
    for (int shard = DATA_SHARDS; shard < TOTAL_SHARDS; ++shard) {
        printf("        \"");
        for (int byte = 0; byte < size; ++byte) {
            printf("%02x", (unsigned)shards[shard][byte]);
        }
        printf("\",\n");
    }
    printf("    ]),\n");
    fprintf(stderr, "%s: 每片编码 %d 字节, 21 种丢片组合通过\n", name, size);
    result = 0;
done:
    for (int shard = 0; shard < TOTAL_SHARDS; ++shard) {
        reed_solomon_free(shards[shard]);
    }
    return result;
}

int main(void) {
    /* 与 Sunshine 和 Moonlight 音频路径一致, 覆盖 nanors 默认矩阵. */
    static const uint8_t parity[] = {0x77, 0x40, 0x38, 0x0e, 0xc7, 0xa7, 0x0d, 0x6c};
    reed_solomon_init();
    reed_solomon *rs = reed_solomon_new(DATA_SHARDS, PARITY_SHARDS);
    if (!rs) {
        fprintf(stderr, "reed_solomon_new 失败\n");
        return EXIT_FAILURE;
    }
    memcpy(rs->p, parity, sizeof(parity));
    printf("const UPSTREAM_PARITY: [(&str, usize, [&str; 2]); 2] = [\n");
    int result = emit_vector(rs, "basis", 4, 1);
    if (result == 0) {
        result = emit_vector(rs, "sweep", 257, 0);
    }
    printf("];\n");
    reed_solomon_release(rs);
    if (fflush(stdout) != 0 || ferror(stdout)) {
        return EXIT_FAILURE;
    }
    return result == 0 ? EXIT_SUCCESS : EXIT_FAILURE;
}
