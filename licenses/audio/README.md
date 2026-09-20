# 音频代码来源与许可

Synly 的桌面音频实现包含来自以下上游的移植和改写. 原作者及贡献者保留各自的著作权. 本文件不表示上游认可或支持 Synly, 也不将上游商标或图标的权利授予 Synly.

| 上游 | 固定源码版本 | 涉及的实现 | 随附许可全文 |
| --- | --- | --- | --- |
| Sunshine / LizardByte 与贡献者 | [40b36212886a914082bfe69cea35210057fc98a1](https://github.com/LizardByte/Sunshine/tree/40b36212886a914082bfe69cea35210057fc98a1) | 系统音频捕获, PCM 转换, Opus 编码配置, 采集/编码线程和 RTP/FEC 发送 | [sunshine-GPL-3.0.txt](sunshine-GPL-3.0.txt) |
| moonlight-common-c / Moonlight Stream 与贡献者 | [62e066388f1a1b133e0bee947b9a374311a3354b](https://github.com/moonlight-stream/moonlight-common-c/tree/62e066388f1a1b133e0bee947b9a374311a3354b) | RTP 音频队列, 启动同步, 乱序, FEC 恢复及丢包占位 | [moonlight-common-GPL-3.0.txt](moonlight-common-GPL-3.0.txt) |
| moonlight-qt / Moonlight Stream 与贡献者 | [49bf1e80da945fc95547d8d64b40d54cbb2f3cb3](https://github.com/moonlight-stream/moonlight-qt/tree/49bf1e80da945fc95547d8d64b40d54cbb2f3cb3) | Opus 多流解码, 播放积压控制, 设备重建与恢复丢帧流程 | [moonlight-qt-GPL-3.0.txt](moonlight-qt-GPL-3.0.txt) |

三个许可文件均逐字保留固定版本仓库中的 GNU GPL version 3 文本, 包含原有无担保条款. 不通过此说明扩大或缩小各上游的许可授权. 这些许可仅覆盖相应来源, 不能代替整个应用及所有依赖的许可清单.

## Synly 中的修改

相关路径为 `src/audio/`, `native/macos_audio.m` 和 `native/macos_*audio*.h` / `native/macos_capture_*.h` / `native/macos_playback_*.h`. 修改者为 Synly 贡献者, 音频重构的修改日期与逐次差异由对应源码版本的 Git 历史记录. 当前实现并非上游软件的原样复制:

- Rust 所有权, Tokio 任务和有界队列替换部分 C/C++ 内存池与线程控制.
- 桌面捕获仍使用 WASAPI/Core Audio. 发布包播放使用 SDL2 renderer 移植; 日常源码构建默认仍是 WASAPI/AudioQueue.
- macOS 增加实际 PCM 格式校验, 无互斥样本环, 清理失败资源隔离, 回调停滞检测和属性通知生命周期管理.
- FEC 接收修正顺序包快速路径漏恢复和序号零值哨兵问题, 保留明确的上游对照差异.
- 传输外层采用 Synly 的 TLS 协商, ChaCha20-Poly1305 和防重放机制, 不提供 GameStream RTSP/AES-CBC 互操作兼容性.

详细函数映射和测试边界见对应 Synly 源码中的 `docs/audio-port.md`, `docs/audio-platform-audit.md`, `docs/audio-fec-vectors.md` 和 `docs/audio-queue-vectors.md`.

独立对照测试使用 nanors 的固定版本, 不将其 C 实现链接进产品. nanors 的 MIT 声明已经随测试源码保留在 `native/tests/audio-fec-nanors-LICENSE.txt`. 重新生成对照数据时仍须保留该声明. Opus 及其它实际链接的依赖必须按发布构建使用的具体版本另外核对, 本目录不是完整依赖清单.

发布包若启用 SDL2 播放, 会额外随附 [SDL2-LICENSE.txt](SDL2-LICENSE.txt). macOS Homebrew 的 sdl2-compat 运行时加载 SDL3, 此时还会随附 [SDL3-LICENSE.txt](SDL3-LICENSE.txt). 这两份是 zlib 许可的运行库全文, 不是 Sunshine/Moonlight 的移植源码.

## 源码与再分发

随附本文件和 GPL 文本只是分发义务的一部分. 分发含 GPL 派生代码的二进制时, 分发者还须按适用条款提供与该二进制对应的完整源码, 包括修改和控制编译/安装所需的脚本. 仅提供上述未修改上游仓库链接不足以提供 Synly 修改后的对应源码.

请以应用显示的构建标识核对分发者同时提供的 Synly 源码版本. 带未提交修改的开发构建不能用基础 commit 的源码归档代替. 当前打包脚本仅保证复制本目录的来源说明和许可文本, 不自动生成或上传完整对应源码, 不构成书面源码要约, 也不证明全部许可义务已满足. 正式分发前仍须核对源码供应方式和完整依赖许可. 不应在缺少这些材料时将打包成功视为可公开发布的证明.
