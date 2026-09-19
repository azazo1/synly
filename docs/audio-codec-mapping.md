# 默认 Opus 声道映射对照

固定源码版本见 [音频移植映射](audio-port.md). 此处仅验证默认编码参数和 Opus 调用, 不宣称 RTSP/GameStream 协议互通或真实扬声器验证完成.

## 上游参数路径

- Sunshine `src/audio.cpp:51-100` 定义六个默认配置, `encodeThread:109-141` 将声道数, streams, coupledStreams 和 mapping 直接传给 `opus_multistream_encoder_create`, 使用 RESTRICTED_LOWDELAY, 固定码率, VBR=0.
- Sunshine `src/platform/common.h:279-320` 的 speaker 顺序为 FL,FR,FC,LFE,BL,BR,SL,SR. stereo/5.1/7.1 的 mapping 分别是对应长度的顺序数组, 不是 Vorbis 声道顺序或旧 GFE fallback.
- 自定义模式 `apply_surround_params:351-355` 会覆盖声道数, streams, coupledStreams 和 mapping. 本次对齐仅针对默认表, 不表示已移植自定义参数协商.

| 模式 | 声道数 | streams | coupled | mapping | 总码率 bit/s |
| --- | --- | --- | --- | --- | --- |
| stereo | 2 | 1 | 1 | 0,1 | 96000 |
| stereo HQ | 2 | 1 | 1 | 0,1 | 512000 |
| 5.1 | 6 | 4 | 2 | 0,1,2,3,4,5 | 256000 |
| 5.1 HQ | 6 | 6 | 0 | 0,1,2,3,4,5 | 1536000 |
| 7.1 | 8 | 5 | 3 | 0,1,2,3,4,5,6,7 | 450000 |
| 7.1 HQ | 8 | 8 | 0 | 0,1,2,3,4,5,6,7 | 2048000 |

所有默认配置均为 48 kHz. `src/audio/config.rs` 现在保留这张表的有效 mapping 前缀; 固定数组中未使用的位置为零. Windows PCM 按 speaker mask 升序排列, 不额外重排此默认映射, 见 [平台审计](audio-platform-audit.md).

## 与 Moonlight 映射的区别

Moonlight common `src/RtspConnection.c:678-714,748-847` 从服务端 `surround-params` 解析映射. 普通质量模式会将描述中末尾的 LFE 移回索引 3; 高质量模式直接使用第二份描述. 只有缺少描述的旧 GFE 5.1 路径使用硬编码 `0,4,1,5,2,3` fallback. Synly 先前将这类映射用于自己的默认环绕声表, 并不等同 Sunshine 的原始编码表.

Sunshine `src/rtsp.cpp:975-985` 在描述中做相反方向旋转以应对 GFE 兼容规则, 但那是协议描述层, 不改变送入编码器的默认 mapping. 固定版本代码的旋转终点使用 `audio::MAX_STREAM_CONFIG` 而不是当前 channelCount; 该枚举在 `src/audio.h:18-26` 为六种模式的数量. 因此不能未经 RTSP 专门测试就假定该表达式对 7.1 也给出预期旋转. Synly 不实现这段描述逻辑, 不将其复制进 PCM/Opus 编码映射.

Moonlight Qt `app/streaming/audio/audio.cpp:69-80` 先让 renderer 对已解析映射做平台重排, 再创建解码器. `renderers/renderer.h:18-26` 的默认实现不重排; SDL renderer 沿用它. 平台重排, 服务端编码映射和 RTSP 兼容处理是不同阶段, 不应混用.

## 独立参数对照

`src/audio/codec/upstream_tests.rs` 使用人工逐项核对固定上游源码后记录的六项参数表和顺序映射, 独立创建参考编码器/解码器. 参考端直接调用已链接 Opus 的 C API, 不从 Synly 的 `CodecConfig` 或 `StreamParams` 派生参数, 不调用 Synly 的编解码包装层.

测试入口为 `just audio-test`, 包括:

1. 六项预设在 5/10/20/40/60 ms 下的声道数, 流数, 码率和映射检查.
2. 30 种组合共 180 帧, 在相同 PCM 和缓冲容量下比较 Synly 与原始 C API 的编码字节. 随后双向交叉解码, 逐位比较 PCM; 每种组合包含一次 PLC 和后续恢复.
3. 六项预设共 32 个逐声道输入场景, 每个运行 12 帧, 跨过编码延迟后检查固定参考解码器的目标索引能量, 其它声道能量低于目标的 1%.

回归测试先在旧映射下运行, 三项均失败: 参数表不符, 5.1 编码字节不同, 右前输入在参考解码器中出现在索引 4. 对齐默认表后再验证通过, 不只验证新代码与自身相互匹配.

这是参数独立的同库对照, 不是独立 Opus 实现, 也没有编译运行完整 Sunshine/Moonlight 应用. 字节一致性限定在同一进程同一 Opus 构建, 不要求跨版本或跨 CPU 输出固定字节. 不覆盖 RTSP 描述旋转, 自定义 surround 参数, 系统混音转换和真实声卡输出.

## 使用边界

`runtime/send.rs` 和 `runtime/receive.rs` 的公开运行入口仍分别创建 `CodecConfig::default()`, 即 stereo. 本次环绕声默认映射变化不改变当前公开会话音频格式或公共协议字段. 多声道会话选择/协商仍待实现, 不为旧实验性环绕声映射添加自动探测或兼容分支. 通过底层测试入口使用环绕声的两端必须使用相同的明确参数, 不能将旧映射的 payload 当成新映射解码.
