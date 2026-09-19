# 接收队列独立对照

## 来源与复现

测试直接编译 [moonlight-common-c RtpAudioQueue.c](https://github.com/moonlight-stream/moonlight-common-c/blob/62e066388f1a1b133e0bee947b9a374311a3354b/src/RtpAudioQueue.c), 不复制或改写上游队列算法. 固定版本为 `62e066388f1a1b133e0bee947b9a374311a3354b`. Reed-Solomon 使用 [nanors](https://github.com/sleepybishop/nanors/tree/b1e3c22ca0cdc0bb83e3cd6ed1a2fc77869ed99a) 的原始 C 实现, 与 [FEC 独立验证](audio-fec-vectors.md) 使用同一版本. Moonlight 代码按 GPL v3 提供, nanors 按 MIT 提供; 分发链接了上游代码的生成器时也须遵守相应许可, 它不是脱离上游许可的独立算法实现.

已有源码目录可用下列入口运行, 不安装依赖, 不下载或修改上游文件:

```shell
just audio-queue-vectors /path/to/moonlight-common-c /path/to/nanors
```

此入口编译 `native/tests/audio-queue-vectors.c` 与原始队列和 nanors, 启用 ASan/UBSan, 运行后通过 `cmp` 比较 `native/tests/audio-queue-vectors.tsv`, 再运行 Rust 对照测试. 常规 `just audio-test` 读取随仓库保存的固定轨迹, 无需上游源码目录或 C 编译器. 上游代码只作为测试工具的显式外部输入, 不成为产品构建依赖.

| 文件 | SHA-256 |
| --- | --- |
| 上游 `src/RtpAudioQueue.c` | `ab339c371e2549787c575f6dbc3042f3be49170783a278c03f5f6fff8c898ae0` |
| 上游 `src/RtpAudioQueue.h` | `4f848bd9504380effa9ffec77c181e1d8220cf9b0608b48f94bbc2a1f6046487` |
| 固定 `audio-queue-vectors.tsv` | `54b419fa97ac9e2a3f2a39fdb587a27b9bfbb90f79ee12334adde80444d6113c` |

## 驱动与证据边界

- 使用现代服务器版本, 48 kHz 默认串流对应的 5 ms 包时长, 4 数据 + 2 校验片, 每片 16 字节. 输入 payload 包含序号的高低位, 防止仅比较低字节掩盖回绕差异.
- 枚举 15 种双丢片组合及剩余 4 片的全部 24 种到达顺序, 共 360 条轨迹. 另有 10 条顺序包, 重复数据/校验包, 校验先到, 整块丢失, 不可恢复块的 PLC, 迟到后等待, 跨块混合, 校验包启动, 接近回绕和回绕初始化轨迹.
- 每次输入后记录输出 payload 字节与 Missing 占位. TSV 三列分别为名称, 输入事件和逐事件输出; `a4` 表示序号 4 的数据包, `f4/1` 表示基序号 4 的第 1 个校验片. 逗号分隔输出帧, 分号结束一次输入事件, 空事件仍保留分号.
- C 驱动将数据 RTP 头转换为队列 API 所需的主机字节序, FEC 头保留网络字节序. 每次添加包后取尽当前可用队列数据, 并把 HANDLE_NOW 的包先计入输出. 这是归一化队列 API 对照, 不是完整 AudioStream.c 接收线程的调度复现; 后者的 HANDLE_NOW 分支不会立即调用 GetQueuedPacket.
- 时钟冻结, 不在此验证超时单位或真实调度延迟. Rust 测试只调整已有块的私有测试时间戳, 不修改生产时钟. 测试不能替代超时, 初始音频丢弃窗口, 无 FEC 旧服务器模式或完整网络层的验证.
- `audio-queue-shim/enet/enet.h` 仅声明 Limelight-internal.h 所需的 3 个不透明 ENet 类型和整数类型, 没有网络实现. 队列不调用 ENet; FEC 和队列逻辑均运行真实上游代码. 驱动只提供版本, 包时长, 关闭的日志回调和固定时钟.
- 编译定义 LC_DEBUG 保留结构断言, LC_FUZZING 关闭会主动改动输入的合成丢包验证模式, 同时关闭上游针对远端输入的 LC_ASSERT_VT 断言. 不依赖合成丢包来获得测试结果. ASan/UBSan 设置为发现问题即失败, 未报告错误.

## 对照结果与明确差异

370 条轨迹均纳入断言, 不忽略未解释的不一致:

1. **273 条逐事件逐字节相同.** 包括常规顺序, 乱序/FEC 恢复, 重复过滤, PLC 和整块缺失跳转.
2. **96 条保留 synly 的及时 FEC 恢复.** 上游 `RtpaAddPacket:602-621` 遇到当前期待的数据包直接返回 HANDLE_NOW, 跳过 `completeFecBlock:647-652`. 如果此包恰好凑齐 4 个有效分片, 且后续还缺数据, C 队列仍不能交付这些可恢复数据. synly 在每次添加后检查 FEC, 不复制这一快速分支的遗漏. 测试根据输入顺序和丢片位置识别该分支, 验证此前事件完全相同, 最后一次输出只追加精确的剩余 payload, 并验证最终 4 个数据帧完整有序. 不把任意差异都视为成功.
3. **1 条保留回绕初始化修正.** 首包为 65532 时, 上游用 oldestRtpBaseSequenceNumber 为 0 判断未初始化, 会在下一块到达时再次同步并丢掉 0..3. synly 的显式 initialized 状态正确交付 0..4, 上游轨迹只交付 4. 两种输出都被明确断言.

例如输入 `a0 a7 f4/0 f4/1 a4` 中, a0 用于启动同步, 后续 a7 和两个校验片先到. 最后 a4 到达后已有 4 个有效分片. 原始 C 队列仅输出 a4, synly 输出 a4..a7. 该差异已经由未修改的上游实现复现, 不能将所有轨迹概括为与上游完全一致.
