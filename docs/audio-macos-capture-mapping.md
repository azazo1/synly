# macOS 系统音频捕获对照

本表针对 Sunshine 提交 `40b36212886a914082bfe69cea35210057fc98a1` 的 `src/platform/macos/av_audio.mm` 已检查部分, 不表示整个文件已逐行移植. 上游还有麦克风捕获路径, 当前 Synly 系统 tap 不提供该路径.

| 上游位置 | Synly 位置 | 当前行为与差异 |
| --- | --- | --- |
| `audioConverterComplexInputProc:76-99` | `native/macos_audio_conversion.h:109` | 以 framesProvided 推进指针, 按请求帧数和剩余量取较小值. 上游耗尽返回 noErr, Synly 返回专用 NEEDS_INPUT 状态, 避免把暂缺输入当作流结束 |
| `systemAudioIOProc:125-160` | `ar_pcm_push` | 上游取第一个 buffer 并假设 float PCM. Synly 校验单 buffer, 源格式, 字节对齐及预分配容量, 不支持的格式返回错误 |
| `systemAudioIOProc:162-175` | `ar_pcm_push`, `ar_system_audio_io_proc` | 上游转换失败或无输出时直接写原始字节. Synly 不把源格式数据当作目标格式, 转换失败通过 ring 报告并进入恢复. 暂时无输出且转换器需要输入时保持转换状态 |
| `systemAudioIOProc:180-207` | `ar_pcm_push`, capture ring | 上游无有效输入时合成最多一帧静音并唤醒. Synly 对 NULL 数据但有字节长度的 buffer 合成等长静音; 无帧输入不合成额外时长, 等待后续输入或健康检查 |
| IOProc 实时约束 | `ar_system_audio_io_proc` | 使用 C 状态和预分配转换内存, 不在回调中执行 Objective-C 调用. Synly 另检查属性变化, 回调重入和健康状态 |

Rust 入口 `src/audio/platform/macos.rs` 在设备调用前校验 Opus PCM 采样率, 声道和时长范围, 检查帧数/样本数的 u32 表示. 输入和输出实例保存协商的样本数, 拒绝非完整帧再调用 C ABI. 这防止长度截断或将部分帧交给原生 ring. 系统 tap 仍限定立体声, 不代表已实现多声道捕获.

上述格式错误和空输入差异需要在最终移植范围中明确保留, 不能仅为了逐行相似而把不同格式的原始字节交给编码器. 初始化, 属性监听, 默认输出切换和清理的完整上游对照仍待补齐. 相关 Rust 边界测试不访问设备, 不能代替 Core Audio 权限和真实声卡验证.
