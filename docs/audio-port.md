# 音频移植映射

目标是将 Sunshine 的电脑系统音频捕获和发送链路, Moonlight 的音频接收, 恢复与播放链路移植到 synly. 当前处于实施阶段, 下表中的已对齐项不代表整个目标完成.

## 上游基线

| 项目 | 固定版本 | 用途 |
| --- | --- | --- |
| [Sunshine](https://github.com/LizardByte/Sunshine/tree/40b36212886a914082bfe69cea35210057fc98a1) | `40b36212886a914082bfe69cea35210057fc98a1` | Windows WASAPI, macOS system tap, Opus 编码和 RTP/FEC 发送 |
| [moonlight-common-c](https://github.com/moonlight-stream/moonlight-common-c/tree/62e066388f1a1b133e0bee947b9a374311a3354b) | `62e066388f1a1b133e0bee947b9a374311a3354b` | 音频包接收, 启动同步, 乱序队列, FEC 和丢包补偿 |
| [moonlight-qt](https://github.com/moonlight-stream/moonlight-qt/tree/49bf1e80da945fc95547d8d64b40d54cbb2f3cb3) | `49bf1e80da945fc95547d8d64b40d54cbb2f3cb3` | Opus 解码, SDL 播放, 延迟控制与设备重建 |

移植代码的来源归属于各上游项目及其贡献者. Sunshine 和 Moonlight 的固定版本 GPL v3 全文与来源说明现随项目保存在 [音频许可目录](../licenses/audio/README.md), 三个文本与上游固定提交逐字一致. 分发移植产物时还须提供匹配二进制的完整对应源码并保留上游声明; 仅随附许可文件不能证明已经满足全部义务.

## 函数级对应

下列行号均对应上述固定版本, 路径相对于各上游仓库.

| 上游源码 | synly 对应 | 状态 |
| --- | --- | --- |
| Sunshine `src/audio.cpp:51-100`, `encodeThread:109-152` | `src/audio/config.rs`, `codec.rs` | 保留 48 kHz 声道数, 码率, LOWDELAY 和 CBR, 默认环绕声 mapping 已对齐顺序数组; 参数独立的直接 C API 对照见 [声道映射对照](audio-codec-mapping.md); ctl 前建立 RAII, 补齐 FFI 参数/长度校验 |
| Sunshine `src/audio.cpp`, `capture:157-274`; `src/thread_safe.h:391-543` | `src/audio/runtime/capture.rs`, `runtime/send.rs`, `runtime/queue.rs` | 采集, 编码, UDP 发送分离; 30 帧采集和 32 包编码队列满时清空旧积压. 设备失败只重建输入, 保留编码, RTP/FEC 和加密状态; 线程优先级仍待补齐 |
| Sunshine `src/stream.cpp`, `audioBroadcastThread:1850-1953` | `src/audio/sender.rs`, `fec.rs` | 已有 4 数据 + 2 校验包及同样的 GF(256) 系数; 已与 pinned nanors C 固定向量逐字节对照, 详见 [FEC 独立验证](audio-fec-vectors.md) |
| Moonlight common `src/RtpAudioQueue.c`, `getFecBlockForRtpPacket:197-399` | `receiver.rs`, `add_packet`, `block_identity`, `ensure_block` | 已补齐旧包过滤前的 OOS 统计, 去重, 已完成块过滤及块标识一致性检查 |
| 同文件 `completeFecBlock:401-507` | `receiver.rs`, `try_complete_block`; `fec.rs` | 数据 + 校验达到 4 片后恢复, 回填缺失数据, 保留已输出分片供剩余分片恢复 |
| 同文件 `handleMissingPackets:517-564` | `receiver.rs`, `handle_missing_packets` | 整块丢失立即重新同步; 部分丢失在下一块到达后按乱序历史决定等待或 PLC |
| 同文件 `RtpaAddPacket:566-660`, `RtpaGetQueuedPacket:662-731` | `receiver.rs`, `add_packet`, `dequeue_ready` | 顺序包立即出队, 丢失占位交给 Opus PLC; Rust 以所有权和 VecDeque 替代 C 链表及内存池 |
| Moonlight common `src/AudioStream.c:248-318` | `receiver.rs`, `AudioDepacketizer`; `runtime.rs` | 启动窗口丢弃数据和 FEC, 仅数据包消耗计数; 收包后出现 socket 超时则结束启动丢弃 |
| Moonlight common `src/AudioStream.c:142-159,385-395`; `src/LinkedBlockingQueue.c` | `runtime/receive.rs`, `runtime/queue.rs` | 独立 UDP 接收与解码播放阶段, 30 包解码队列, 溢出清空并接受最新包 |
| Moonlight Qt `app/streaming/audio/renderers/sdlaud.cpp:103-125` | `runtime/render.rs`, `decode_frames`; 原生平台 ring | 网络积压超过 30 ms 丢 PCM; 原生软件队列用精确 50 ms 水位和最长 100 ms 等待, 不复制 SDL 整包取整与耗尽后继续入队. 原始 C++ 对照见 [renderer 行为对照](audio-renderer-oracle.md) |
| Moonlight Qt `app/streaming/audio/audio.cpp:165-180,225-253` | `runtime/render.rs`, `runtime/queue.rs` | 播放失败后释放设备和解码器, 按重建耗时设置丢帧窗口; 初始无设备可重试, 使用固定 1 秒而非 200 包计数 |
| Sunshine macOS `av_audio.mm:70-98,125-175` | `native/macos_audio_conversion.h`, `macos_audio.m` | 移植输入提供和转换流程, 修正回调暂缺被当作 EOF 的语义; 查询实际 ASBD, 有界排空输出, 通过真实 AudioConverter 分块对照 |
| Sunshine Windows `audio.cpp:719-726,954-973,987-1008` | `src/audio/platform/windows/endpoint.rs`, `windows.rs` | 默认输出与固定 endpoint ID 分流, ACTIVE 状态校验, 固定重建保留 ID 且不跟随默认切换; 友好名称/虚拟 sink 解析及通知回调仍未移植 |
| Sunshine Windows `audio.cpp:60-130,326-397` | `src/audio/platform/windows/format.rs`, `stream.rs` | 2/6/8 声道 float WAVEFORMATEXTENSIBLE, 精确 ABI 与 GetMixFormat 生命周期; 只采用兼容 speaker mask, 不盲从自定义布局; 会话协商和硬件验证未完成 |
| Sunshine Windows/macOS 捕获与 Moonlight Qt 播放 | `src/audio/platform/`, `native/macos_audio.m` | 详见 [平台审计](audio-platform-audit.md), 不应视为完成 |

## 有意保留的边界和差异

- 网络 I/O 使用 Tokio UDP, 原生采集, Opus 编码和解码/播放分别占用阻塞工作任务. `runtime/workers.rs` 在任一工作任务退出, panic 或主动取消时关闭所有队列, 唤醒消费者并等待回收. 可恢复的设备错误分别在 `runtime/capture.rs` 和 `runtime/render.rs` 内处理, 不退出工作任务. 这替代上游手动 thread/event 生命周期, 没有改变队列溢出丢弃策略.
- 原生软件播放队列使用 50 ms 提交前水位, 容量额外容纳一完整协商帧. Windows 条件变量和 macOS Mach 通知替代 SDL 的 1 ms 轮询, 最长等待 100 ms 后返回错误, 不复制上游超时后继续入队的行为. 捕获按设备块长补足拼帧余量. 这些是 WASAPI/AudioQueue 的行为适配, 不是 SDL renderer 的逐行替换. 原始 `sdlaud.cpp` 在设备替身上的 6 组 ASan/UBSan 测试确认了整包取整水位, 100 次等待后继续排队和 QueueAudio 错误仍返回成功的上游行为, 详见 [renderer 行为对照](audio-renderer-oracle.md).
- Windows backend 已接受 2/6/8 声道 PCM, 流初始化使用 WAVEFORMATEXTENSIBLE 和兼容 native speaker mask. 当前默认会话仍为 stereo, macOS tap 仍限定 stereo, 未增加多声道 GUI/CLI 选择或跨平台协商. 默认环绕声 Opus mapping 已与固定 Sunshine 编码表对齐, 通过独立参数的原始 C API 编码字节和交叉解码对照, 不等于已实现 RTSP/GFE 描述兼容或自定义 surround 协商; 详见 [声道映射对照](audio-codec-mapping.md) 和 [平台审计](audio-platform-audit.md).
- Opus 数据帧必须与协商的每声道样本数一致, 解码前先检查首个流的 TOC, 防止短帧推进解码状态后才报错. 这是 synly 的固定时长串流约束; 声道数, 映射, 流数和缓冲区长度均在进入 FFI 前校验.
- 音频 UDP 外层仍使用 TLS 会话导出的 ChaCha20-Poly1305, 但每次接收端绑定都会生成 32 字节 channel_id 并通过 TLS 控制通道交付, 以该标识派生本次方向密钥. 包内计数器使用 64 位且不可回绕, 接收端允许 64 包乱序窗口并拒绝重复和窗口外重放. 旧音频通道的包不能注入新绑定. 这不是 GameStream AES-CBC/RTSP 握手, 仍不与 Moonlight/Sunshine 服务端直接互通.
- `AudioUdpReady` 的 channel_id 是必需字段, UDP AAD 和派生域使用 v2, 没有旧音频格式回退. 桌面两端须同步更新. 发送任务在同一已协商通道中只创建一次, 设备内部恢复必须保留其计数器; 需要重建整个音频任务时, 必须先重新绑定接收端并经 TLS 协商新标识.
- 捕获端对打开/读取的 Backend/Io 错误采用 5 秒退避, 首次没有设备也会重试; 与 Sunshine 不同, 读取错误后同样退避以防设备抖动引发忙循环. 普通读取超时继续读取; macOS 若连续 5 秒没有 IOProc 回调则报告 Backend 错误并重建, 不按样本音量判定停滞. 失败的部分帧被丢弃. 保留完整帧队列, 编码器和发送任务, 不生成补偿静音也不轮换通道密钥. 退避可取消, 系统设备 API 本身的阻塞仍须等其返回.
- macOS 双向音频共用独立的属性通知信号模块. 捕获监听输入 stream/格式/缓冲/存活状态和默认输出变化, 播放监听默认输出变化, 分别使旧实例失效并进入现有工作线程重建流程. 通知 block 不持有捕获/播放对象或 ring, 注销失败仍进入永久隔离. 这是 synly 新增的安全恢复行为, 不代表已完成真实设备切换验证, 详见 [平台审计](audio-platform-audit.md).
- macOS 原生清理失败进入资源隔离状态, 保留可能仍被回调访问的上下文, 禁止进一步分配或创建设备. `BackendFatal` 不触发自动重试, 需要重启应用; 生命周期故障注入与保留资源的代价见 [平台审计](audio-platform-audit.md).
- synly 每次添加分片都尝试 FEC 恢复, 不复制上游顺序包 HANDLE_NOW 快速返回时跳过恢复的行为. 独立运行原始队列已复现 96 种受影响的双丢片/乱序组合, 详见 [接收队列独立对照](audio-queue-vectors.md).
- 接收初始化使用独立布尔状态, 避免上游以 `oldestRtpBaseSequenceNumber == 0` 为哨兵时在 65532 -> 0 回绕处再次同步.
- 同一个 FEC 块的时间戳, SSRC, payload type 和长度不一致时拒收. 固定 RTPv2 头以外的形式也拒收, 不将额外头字段交给 Opus 或 FEC.
- 上游当前 `handleMissingPackets` 使用微秒时钟, 但表达式中的包时长仍是毫秒. synly 显式使用 `packet_duration_ms * 4 + 10 ms` 的时间预算, 不复制单位不一致的问题.
- 每次出队和 socket 超时都可推进已到期的恢复状态, 避免网络停止后只能等新包到来才能输出已排队数据. 仍要求第二个 FEC 块存在才放弃当前块, 不无限生成静音.
- 无效 RTP/FEC 被丢弃并记录 debug 诊断; Opus 数据解码失败尝试 PLC. 播放 Backend/Io 错误只触发设备和解码器重建, 首次没有设备也每隔 1 秒重试. 重建期间及其后的恢复窗口丢弃旧积压, 配置和编解码错误则停止链路. Windows WASAPI 播放线程不再内部重建, `Restart` 会关闭旧 ring 并报告错误, 统一进入该流程. 详见 [平台审计](audio-platform-audit.md).
- Android 的 Rust 客户端当前在 `crates/synly-core/src/client.rs` 的 `client_workspace_summary` 和 `run_session` 中固定协商 `AudioMode::Off`. 桌面音频模块不由 Android 核心编译, 当前 Android 不参与这条音频传输链路. 后续公共协议变更需保持该能力边界一致, 不应仅凭桌面测试宣称 Android 音频已实现.
- 尚未移植上游旧 GeForce Experience 的无 FEC 兼容模式. synly 当前发送端固定使用现代 4+2 数据布局.

## 许可随附验证

桌面打包脚本在归档前复制 `licenses/audio/` 中的三个上游 GPL 全文和来源说明. macOS 放入 `Synly.app/Contents/Resources/audio-licenses/`, Linux TAR 与 Windows ZIP 放入根目录 `audio-licenses/`. 固定清单中的文件缺失或为空时打包失败; 暂存复制不会扫描用户配置或打包本机参考仓库. 许可目标必须为新目录, 避免混入旧文件. Windows 中文脚本使用 UTF-8 BOM 以兼容 Windows PowerShell 5.1.

`just audio-notices-test` 验证上游全文 SHA-256, 逐字复制, 输入缺失/空文件和目标冲突的拒绝行为. Unix 测试还拒绝许可输入的符号链接, 用只供格式检查的 ELF 占位文件执行真实 Linux 归档脚本并解包比对; Windows 对 MZ 占位文件执行真实 ZIP 打包和解包. 测试从不运行这些占位文件, 不属于跨平台编译或应用启动验证. macOS 测试覆盖 `.app` 内许可资源布局及脚本语法, 尚未生成并挂载真实 DMG 检查.

当前未自动生成或上传与发布二进制匹配的完整对应源码, 也未审计所有实际链接依赖及整个应用的许可兼容性. 脏工作区源码, 依赖源码和构建/安装脚本的可获得性仍须验证. 此处不做项目整体重新许可, 不创建源码书面要约, 不把许可证随附测试当作可以公开发布的法律结论.

## 验证与剩余工作

音频测试位于 `src/audio/receiver/tests.rs`, `src/audio/codec/tests.rs` 和 `src/audio/runtime/` 的测试模块, 运行入口为 `just audio-test`. macOS 为 63 项, Windows 为 108 项音频测试通过, 两平台另有 1 项公共音频控制帧测试通过, macOS 和 Windows 的 `cargo clippy --offline --all-targets --all-features` 均通过且无警告. 测试覆盖全部 15 种双包丢失组合, 真实 Opus 编解码/CBR/声道映射/PLC, 工作任务失败与取消回收, 30 包溢出清空, 解码积压丢弃, 合成 PCM 经完整发送流水线后的 UDP/RTP/FEC 解码, 暂停播放与阻塞设备重建时 UDP 仍能收包. 恢复测试还验证初始无设备重试, 旧设备先销毁, 解码器重置, 重建积压清理和无网络时的计时/取消; macOS 的 7 项播放恢复测试额外重复 10 次通过. 测试使用真实本机 UDP 与 Opus, 音频设备由可控输入/输出替代, 不代表真实声卡和设备切换已验证.

捕获恢复另以完整发送任务经历实际 5 秒退避, 验证跨设备重建的 RTP 序号, SSRC, 时间戳和 AEAD 计数连续, 以及恢复后的第 4 帧补齐原 FEC 块. Opus payload 与不中断的独立编码器逐字节一致; 失败的部分帧和取消后完成的帧不进入发送链路. macOS 不支持的指定设备参数和 Windows 非法 endpoint ID 在平台入口返回永久错误, 不陷入设备重试. Windows 有效格式但暂时离线的固定 endpoint 属可恢复设备错误, 不回退默认设备; 固定选择与默认轮询的 COM 替身测试见 [平台审计](audio-platform-audit.md).

原生平台测试详见 [平台审计](audio-platform-audit.md). macOS 的 `just audio-native-test` 使用 ASan/UBSan 运行 3 组回调与资源测试, 2 组真实 AudioConverter 测试, 4 组捕获 SPSC 测试, 4 组播放 SPSC 测试, 5 组原生生命周期/停滞/双向属性通知集成测试及 3 组捕获健康状态测试. `just audio-ring-race-test` 使用 TSan 验证两方向共用的 SPSC 存储, 各自的等待策略, 创建/失败清理的串行化及心跳/超时竞争. 旧 pthread ring 和对应测试已经删除. 验证资源回收, 回调自身代码的 mutex/分配禁用, 捕获拼帧, 播放水位, 重采样连续性及输入内存生命周期, 不打开音频设备. Windows 平台测试还验证流代次清理, MMCSS 回收, 初始化调用顺序, 真实事件复位及阻塞提交跨恢复边界的行为.

- [x] 与独立的上游 C 实现对照 FEC 和 RTP 接收队列, 保留固定向量与复现生成器. 队列的 370 条轨迹中, 273 条完全一致, 96 条明确保留及时 FEC 恢复, 1 条保留回绕初始化修正, 详见 [接收队列独立对照](audio-queue-vectors.md). 超时和完整 AudioStream 调度不在该对照范围内.
- [x] 采集/编码/网络/解码阶段分离, 有界队列和网络积压丢弃. 原生设备内部缓冲限制仍见平台待办.
- [x] Opus 配置校验, 初始化失败释放与真实编解码边界测试. 六项默认预设已对齐固定 Sunshine, 30 种模式/时长组合的直接 C API 编码字节, 交叉解码和 PLC 对照通过, 详见 [声道映射对照](audio-codec-mapping.md).
- [ ] 完成平台捕获与播放映射中的待办项.
- [ ] 核对 Android 现有能力协商与所需播放接口, 在允许的 Windows 环境构建验证.
- [x] 同一 TLS 会话重绑音频时轮换通道密钥, 计数器不回绕, UDP 重放窗口与来源绑定测试.
- [ ] 真实双向会话和断开/重连验证.
- [x] 随附固定上游 GPL 全文和音频来源说明, 验证 Linux/Windows 归档包含这些文件.
- [ ] 核对完整对应源码供应, 全部依赖许可与整体兼容性, 并验证真实 macOS DMG 中的许可资源.
- [ ] Windows/macOS 设备切换, 静音, 断流, 丢包及长时间延迟实测.
