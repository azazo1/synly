# 音频平台移植审计

## 范围与定位约定

审计覆盖 `src/audio/platform/`, `native/macos_audio.m`, `src/audio/capture.rs` 和 `src/audio/playback.rs`, 对照 Sunshine 的 Windows/macOS 捕获实现及 Moonlight Qt 的音频渲染实现. 不包含编码器, 网络协议, RTP/FEC, 接收端抖动缓冲和 Android 原生音频链路的完整审计.

下文的 `Sunshine:` 和 `Moonlight Qt:` 路径均相对于对应上游源码仓库. 行号来自初始审计快照, 可能随修改漂移; 后续应同时通过文件名和函数名定位. 本文不代表已完成设备实测.

Windows 当前使用自行实现的 WASAPI 捕获和播放. macOS 已实现 Sunshine 的 system tap, aggregate device 和 AudioConverter 主流程, 播放使用 AudioQueue. Moonlight Qt 桌面默认使用 SDL 渲染器, 见 `app/streaming/audio/audio.cpp:20-51`. Steam Link 的 SLAudio 路径不是 Windows/macOS 桌面的移植目标.

## 已实施修复

以下项目已修改代码并做静态复核. macOS 和 Windows 均已通过主程序音频测试所需的原生编译及全目标, 全功能 Clippy. 测试未打开真实音频设备; macOS 原生失败注入与 Windows Win32 事件及模拟 COM 测试见下文. 设备切换和实际延迟仍需单独验证.

- [x] Windows 音频就绪事件改为自动复位. 初始位置为 `src/audio/platform/windows.rs:395,481,1209-1210`. `CaptureThreadContext::start` 和 `PlaybackThreadContext::start` 使用 `create_auto_reset(false)`, 两个停止事件继续使用 `create_manual_reset(false)`. 上游对应 `Sunshine: src/platform/windows/audio.cpp:607`, `mic_wasapi_t::init` 中的 `CreateEventA(nullptr, FALSE, FALSE, nullptr)`. 原手动复位事件没有 `ResetEvent`, 第一次通知后会使等待循环持续立即返回.
- [x] Windows COM guard 最后析构. 初始位置为 `src/audio/platform/windows.rs:381-388,466-474,1172-1175`. `_com` 已移到两个上下文结构体的末尾. Rust 按字段声明顺序析构, 因而所有 COM 接口先 `Release`, 然后才 `CoUninitialize`. 构造失败路径中的局部 guard 仍先创建, 后于接口局部变量析构. 上游资源包装参考 `Sunshine: src/platform/windows/audio.cpp:227-239,286-295`; 该错误来自 Rust 生命周期适配, 不是上游同名字段问题.
- [x] macOS ring 部分初始化失败可安全清理. 初始位置为 `native/macos_audio.m:60-90,332-345,499-524`. `ARFloatRing::initialized` 仅在数据区和三个 pthread 同步对象全部初始化成功后设为真. 三个 pthread 初始化返回值均已检查, 失败时按逆序释放已创建资源. `ar_ring_close` 和 `ar_ring_free` 跳过未完成初始化的 ring, 正常释放后清零状态. 上游清理边界参考 `Sunshine: src/platform/macos/av_audio.mm:467-505,540-552`.

### macOS ring 生命周期复核

| 路径 | 初始化和失败行为 | 清理前提 |
| --- | --- | --- |
| 捕获构造的声道或系统版本检查失败 | 尚未调用 `ar_ring_init`, 对象零初始化使 `initialized` 为假 | 析构中的 close/free 均不触碰 pthread 对象 |
| `calloc` 失败 | 返回 false, 没有创建同步对象 | close/free 跳过 |
| mutex 初始化失败 | 释放数据区并返回 false | 不销毁未初始化的 mutex/cond |
| 读取条件变量初始化失败 | 销毁 mutex, 释放数据区, 返回 false | 不销毁未初始化的 cond |
| 写入条件变量初始化失败 | 销毁读取条件变量和 mutex, 释放数据区, 返回 false | 不销毁未初始化的写入条件变量 |
| 捕获 ring 初始化成功, 后续 tap/converter/IOProc 初始化失败 | 捕获构造检查 false 并返回 nil; 后续失败由对象析构释放资源 | 先 close, 停止并销毁已创建的 IOProc, 再 free ring |
| 播放 ring 初始化失败 | `ar_macos_playback_create` 检查 false, 直接释放 engine | init 已自行回收部分资源 |
| `AudioQueueNewOutput` 失败 | free 已成功初始化的 ring, 再 free engine | 尚未启动播放 |
| `AudioQueueAllocateBuffer` 或 `AudioQueueStart` 失败 | 先 `AudioQueueDispose(..., true)`, 再 free ring 和 engine | 队列清理调用先于 ring 释放 |
| 正常播放销毁 | close ring, `AudioQueueStop/Dispose`, free ring, free engine | 保持队列清理先于 ring 释放 |

`ar_ring_init` 的两个调用点都检查其 bool 返回值. `ar_ring_close/free` 返回 void. 当前调用方仍忽略 Core Audio stop/dispose 等清理 API 的返回值; 此表只确认调用顺序, 不证明这些 API 失败时回调已经终止. `initialized` 不提供并发销毁保障, ring 的最终释放仍要求所有使用方已停止.

### macOS PCM 转换边界

`native/macos_audio_conversion.h` 对应 Sunshine `av_audio.mm:70-98,125-175` 的输入提供与转换流程, 并按 AudioConverter 输入回调契约修正暂缺数据处理. SDK 指出输入回调返回非零状态和 0 包可暂停转换, `FillComplexBuffer` 同时返回此前已产出的输出. 成功返回 0 包用于真正的流结束, 不能在每次设备回调末尾使用.

- 初始化查询实际输入 stream 的 `kAudioStreamPropertyVirtualFormat`, 校验 native packed Float32, 单个 buffer, 声道数, 每帧/每包字节数和有限采样率. 不再用设备 nominal rate 和第一个 buffer 的声道数构造猜测格式. 多 stream, planar 或整数 PCM 目前明确拒绝, 尚未实现格式转换支持.
- `ar_pcm_push` 将暂缺状态视为正常边界并保留已产出的样本, 输出块满时继续调用转换器. 必须等转换器再次请求输入后才结束设备回调, 避免下一次 IOProc 覆盖转换器仍借用的输入内存. 无进展或超过输出预算时返回错误, 不回退为原始格式数据.
- 预分配依据实际设备输入块, 输出容量属性, 采样率比和 prime 信息, 不再使用固定 `+32` 帧余量. 单次 IOProc 的接受上限为两个预分配输出块, ring 另加一完整读取帧. 这是明确的运行预算, 超限会停止链路, 不是对所有 Core Audio 版本最坏输出量的证明.
- NULL 数据但有合法帧数时提供等长零样本并走同一转换器. 空回调不制造一整编码帧, 以免改变时间线. 超过打开时的最大输入块, 非整帧字节数或声道变化均报告错误. 运行中格式/缓冲变化的自动重建仍待完成.

`just audio-native-test` 同时运行故障注入和真实 AudioConverter 测试, 全部启用 ASan/UBSan. 真实测试将半秒输入分别按整块, 512 帧, 不等长块和强制每次仅输出 7 帧转换到 48 kHz 双声道, 比较完整帧数与逐样本结果. 已覆盖 8/44.1/48/96/192 kHz 双声道和 44.1 kHz 单声道, 每次输出 24,000 帧, 与整块参考的差异小于 0.00001. 每次提交后立即用 NaN 覆盖借出的输入, 验证后续转换不再引用它. 另验证格式拒绝, 静音帧数和两次缓冲分配失败的资源回收. 测试不创建 tap, 不验证真实设备的 stream 查询与切换.

## 待完成问题与上游映射

### P1: 延迟和恢复行为

- [x] 以时间预算约束原生软件缓冲. 捕获以 30 ms 为基准, 并保留一完整协商帧加实际设备包的拼帧空间, 避免 40/60 ms 帧在回调块长不整除帧长时正常丢样. Windows 依据 WASAPI 实际 buffer frames, macOS 依据实际设备 buffer frames 的转换输出上界. 播放容量为 50 ms 加一完整帧. 捕获溢出按完整声道帧保留最新数据, 累计丢弃样本和最高水位, 关闭时输出 debug 统计. 这对应 Sunshine 按设备包与帧长分配缓存的意图; 30 ms 不是所有格式和设备下的容量硬上限.
- [x] Windows shared event 初始化请求默认缓冲. 捕获与播放均向 `Initialize` 传入 0/0 时长, 随后绑定事件, 查询实际 buffer frames, device period 和 stream latency 并记录. `windows/stream.rs` 统一处理, 不将请求值当作实际延迟. 尚未移植 SDL renderer 的设备缓冲选择.
- [x] 移植播放软件队列背压. 对照 `Moonlight Qt: app/streaming/audio/renderers/sdlaud.cpp:103-125`, 网络积压超过 30 ms 时先丢 PCM, 原生提交前软件队列超过 50 ms 时等待消费. 条件变量替代 1 ms 轮询, 总等待不超过调用方 timeout 和 100 ms 中较小者. 50 ms 水位允许再接收一整帧, 故默认 5 ms 帧最大软件积压为 55 ms, 60 ms 帧为 110 ms. 超时返回错误, 与上游等待结束仍入队不同, 保持 synly 缓冲有界. 此水位不包括 WASAPI/AudioQueue 已提交设备缓冲, 不能当作端到端延迟上限.
- [x] Windows 捕获原生 ring 在重建边界丢弃旧音频. `begin_recovery` 清空 ring 并提高 generation, 恢复期间拒收, `finish_recovery` 再清空并允许新提交. 队列原语还验证旧代次提交即使在恢复后醒来也不会混入新样本. 播放端现直接销毁旧 ring, 不再在同一原生线程内重建.
- [x] 播放线程设备恢复与丢帧窗口. `runtime/render.rs` 对照 `Moonlight Qt: app/streaming/audio/audio.cpp:225-253`, 播放 Backend/Io 失败后销毁旧设备和解码器, 等待 1 秒再打开, 初始无设备同样重试. 成功后清理重建期间积压, 再按本次打开耗时设置丢帧窗口, 之后新建 Opus 解码器. 重试按单调时钟而非上游 200 包计数, 网络停流时仍可恢复; 配置, 编解码和不支持平台错误直接退出. UDP, RTP/FEC, 来源绑定及 AEAD 重放状态不重启.
- [x] 捕获失败后只重建设备, 保留发送状态. `runtime/capture.rs` 对照 `Sunshine: src/audio.cpp:248-272`, 先释放失败的输入对象, 再重试创建. synly 对首次无设备和读取 Backend/Io 错误都退避 5 秒, 不复制上游首次失败后只等待停止的行为, 也避免读取立即失败时反复打开. 超时不重建, 部分失败帧不入队; 重试等待可被取消. 编码器, RTP/FEC 和 AEAD 计数器不重新创建, 已完整采集的有界队列仍由编码线程处理. Windows 原生捕获的代次恢复继续保留.
- [x] 永久能力错误不进入设备重试. 两平台尚未实现的指定设备功能返回 UnsupportedPlatform, macOS 在进入 tap 创建前检查系统至少为 14.2. 该版本要求来自 SDK 的 AudioHardwareCreateProcessTap 声明. 不把配置错误当作设备暂时断开.
- [ ] 设备变化通知与真实恢复验证. 运行中格式改变, 默认设备切换, 以及没有错误但回调停止的情况仍需完善; 普通 Timeout 不足以区分静音和设备停止.
- [x] macOS 回调故障交付给工作线程. 转换或 `AudioQueueEnqueueBuffer` 失败时, ring 保存首个 OSStatus 和静态操作名, 关闭队列并唤醒等待读写. 错误字符串在读写线程格式化; Rust 捕获和播放工作线程分别执行设备重建, 不在实时回调中创建或销毁设备. 转换成功但没有输出时不人为插入一帧静音. 初始化阶段的 enqueue 返回值也会检查. 已用 `just audio-native-test` 注入转换和 enqueue 失败, 在 ASan/UBSan 下通过.
- [x] 播放恢复控制不进入实时回调. 原生失败回传到 `runtime/render.rs` 后释放旧输出, 在独立阻塞工作线程重建 WASAPI/AudioQueue. `FrameQueue::discard_until` 用条件变量持续清理待解码包, 无网络输入时仍到期, 监督器关闭队列可唤醒重试和恢复窗口. 系统打开/关闭 API 本身若阻塞, 仍须等待它返回, 不宣称可强制中断设备调用.
- [ ] 真实设备失效, 拔插及默认设备变化后的双平台恢复验证. 现有测试使用真实 UDP, Opus 和线程调度, 但设备打开/提交是可控替身.

### P2: 实时安全与可观测性

- [ ] 消除 macOS 实时回调的阻塞互斥. 初始 `native/macos_audio.m:103-123,126-168,296-314,541-544` 在生产与消费路径共享 pthread mutex, 且持锁逐样本复制. `Sunshine: src/platform/macos/av_audio.mm:114-207` 使用预分配转换缓存, `TPCircularBufferProduceBytes` 和信号通知. 建议单生产者单消费者环, 非阻塞回调, 按完整 sample-frame 处理溢出. Windows 静音包已直接写零, 不再逐包分配临时 Vec.
- [x] 修复 macOS 错误字符串的数据竞争. 创建和读写失败信息使用调用线程本地缓冲, `ar_macos_copy_error` 有界复制到 Rust 提供的存储; 回调错误通过实例 ring 的 OSStatus 传递. 不再返回共享可变裸指针. 故障测试验证线程间错误隔离, NUL 终止, 复制边界和读写线程获得回调故障码.
- [x] Windows 捕获和播放线程注册 MMCSS. `windows/scheduling.rs` 以线程级 RAII 注册 `Pro Audio`, 失败时记录警告并继续普通优先级, 退出时撤销; guard 不跨线程移动. 对应 Sunshine `mic_wasapi_t` 的注册/撤销路径, 已覆盖成功和失败的模拟测试.
- [ ] 补齐持续排队时长和恢复耗时观测. 两平台已在关闭时报告 ring 丢弃样本和最高水位, Windows 还累计恢复丢弃和背压超时样本. `windows/diagnostics.rs` 的 `DATA_DISCONTINUITY` 首次后最多每 5 秒输出一次, 跨重建保留. 连续时长观测和完整恢复耗时仍未覆盖.

### 功能对齐边界

- [ ] Windows 指定 endpoint 和默认设备跟随模式. 初始 `windows.rs:125-130,981-1039` 拒绝指定设备并始终使用默认输出. `Sunshine: src/platform/windows/audio.cpp:630,719-726,757-768,954-973,987-1018` 区分固定 sink 与默认设备跟随, 并使用通知回调. 固定 sink 不应因无关默认设备变化而切换.
- [ ] Windows 多声道及声道布局. 初始 `windows.rs:132-136,203-213` 仅允许双声道 WAVEFORMATEX. `Sunshine: src/platform/windows/audio.cpp:326-345,369-384` 支持 2/6/8 声道和 WAVEFORMATEXTENSIBLE, 相同声道数时采用设备 channel mask. `Moonlight Qt: app/streaming/audio/renderers/renderer.h:18-26` 与 `app/streaming/audio/audio.cpp:69-80` 定义解码映射边界. 必须同步编码映射, 协商参数和播放布局, 不能只删除双声道检查.
- [ ] 宿主扬声器策略. 初始 `native/macos_audio.m:369` 固定 `CATapUnmuted`. `Sunshine: src/platform/macos/av_audio.mm:647-654` 根据 `hostAudioEnabled` 决定静音, `src/platform/macos/microphone.mm:99-100` 传入该选项. Windows 虚拟 sink 的发现, 格式, 切换与还原见 `Sunshine: src/platform/windows/audio.cpp:871-913,1034-1127,1254-1379`. 将路由策略与 PCM 捕获分开, 不在普通 `open_input` 中隐式安装驱动或改变系统默认设备.
- [ ] 明确 macOS 指定输入源是否属于目标. 初始 `src/audio/platform/macos.rs:11-20` 只允许系统 tap. `Sunshine: src/platform/macos/microphone.mm:103-135` 在指定 sink 时使用权限请求与 AVFoundation 输入设备, 相关实现见 `src/platform/macos/av_audio.mm:28-62,263-350`. 电脑系统音频第一阶段可以只实现 tap. 上游 tap 本身始终使用 stereo, 见 `av_audio.mm:628-629`, 不能宣称其拥有真实多声道 system tap.
- [ ] 扩展公共状态与队列接口. 初始 `src/audio/capture.rs:6-13` 只有 Ok/Timeout, `src/audio/playback.rs:6-7` 只有 submit_frame. 需要明确 Reinit/Discontinuity, 流代次, queued duration 和 flush 的责任归属. 接口修改需同步考虑 Android 的独立实现; `src/audio/platform/mod.rs` 中非 Windows/macOS 的 Rust 分支当前返回 unsupported, 不代表 Android 整个应用没有音频支持.

## 推荐实施顺序

1. 完成并验证上述资源生命周期与事件修复. 仅代码检查不足以替代 Windows 和 macOS 平台验证.
2. 固定平台 PCM 边界: interleaved f32, sample rate, channel layout, 每声道帧数, 流代次和不连续状态. 将 endpoint 选择, 原生 stream, PCM queue 分成各自职责.
3. 对照 Sunshine 的 `make_audio_client`, `mic_wasapi_t` 及 macOS tap 初始化, IOProc, cleanup 逐函数实现. Windows 的手写 COM vtable 可在专门改动中改用标准 bindings, 不与行为修复混在一起.
4. 若目标要求逐代码移植 Moonlight 桌面播放, 优先移植 SDL renderer 及其会话层恢复策略. 若继续使用原生 WASAPI/AudioQueue, 应明确属于按相同行为实现, 并逐项实现背压, 设备失效恢复, 丢帧窗口与延迟观测.
5. 在基本实时链路稳定后补齐指定 sink, 多声道布局和宿主音频路由. 编码和网络协议的完成状态由独立审计和验证决定.

## 上游代码的保留意见

`Sunshine: src/platform/macos/av_audio.mm:167-170` 在转换失败时输出原始格式数据. 当源采样率或声道数不同, 该路径会违背输出格式约定, 不应照搬. synly 现在保存转换错误并关闭捕获 ring, 让工作线程收到原始状态码并重建设备; 格式切换的真实设备行为仍待验证.

上游仅使用第一个 AudioBuffer 并假定 packed f32, 固定 `frame_size * 8` 分配见 `Sunshine: src/platform/macos/av_audio.mm:125-150,778-821,843-846`. synly 当前先验证实际 ASBD, 再执行有界分块转换, 详见上文 PCM 转换边界. 仍需在真实设备上验证输入 stream 查询, 以及格式或缓冲大小变化后的重建.

## 最小验证清单

- [x] WASAPI 就绪事件一次等待消费后恢复阻塞; 停止事件保持有信号状态, 已通过 Windows 真实 Win32 事件测试.
- [ ] Windows 初始化失败和正常停止均在 COM 接口释放后执行 `CoUninitialize`.
- [x] macOS 数据区分配和三个 pthread 初始化步骤逐点失败时, 释放已创建资源并保留失败信息; 使用实际生产实现的故障注入测试在 ASan/UBSan 下验证.
- [ ] 捕获和播放后续初始化失败时, 不在回调仍使用 ring 时释放其内存.
- [x] 44.1 kHz 到 48 kHz 等真实转换的分块连续性, 暂缺与静音保持帧数约定.
- [ ] 真实设备不连续, stream 查询与格式切换后的 PCM 约定.
- [ ] 切默认设备, 拔设备和恢复设备后能够恢复, 且不播放旧队列.
- [ ] 持续运行和突发网络积压时, 排队时长可观测且不会无界增长.
- [ ] 启用 5.1/7.1 后以逐通道脉冲验证编码映射与播放布局.
