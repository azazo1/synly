# 音频平台移植审计

## 范围与定位约定

审计覆盖 `src/audio/platform/`, `native/macos_audio.m`, `src/audio/capture.rs` 和 `src/audio/playback.rs`, 对照 Sunshine 的 Windows/macOS 捕获实现及 Moonlight Qt 的音频渲染实现. 不包含编码器, 网络协议, RTP/FEC, 接收端抖动缓冲和 Android 原生音频链路的完整审计.

下文的 `Sunshine:` 和 `Moonlight Qt:` 路径均相对于对应上游源码仓库. 行号来自初始审计快照, 可能随修改漂移; 后续应同时通过文件名和函数名定位. 本文不代表已完成设备实测.

Windows 当前使用自行实现的 WASAPI 捕获和播放. macOS 已实现 Sunshine 的 system tap, aggregate device 和 AudioConverter 主流程, 播放使用 AudioQueue. Moonlight Qt 桌面默认使用 SDL 渲染器, 见 `app/streaming/audio/audio.cpp:20-51`. Steam Link 的 SLAudio 路径不是 Windows/macOS 桌面的移植目标. 固定上游 `sdlaud.cpp` 已在不链接 SDL/Qt 的设备替身上直接编译运行, 6 组 ASan/UBSan 行为证据和逐函数差异见 [renderer 行为对照](audio-renderer-oracle.md); 原始 SDL renderer 尚未进入产品播放路径.

## 已实施修复

以下项目已修改代码并做静态复核. macOS 和 Windows 均已通过主程序音频测试所需的原生编译及全目标, 全功能 Clippy. 测试未打开真实音频设备; macOS 原生失败注入与 Windows Win32 事件及模拟 COM 测试见下文. 设备切换和实际延迟仍需单独验证.

- [x] Windows 音频就绪事件改为自动复位. 初始位置为 `src/audio/platform/windows.rs:395,481,1209-1210`. `CaptureThreadContext::start` 和 `PlaybackThreadContext::start` 使用 `create_auto_reset(false)`, 两个停止事件继续使用 `create_manual_reset(false)`. 上游对应 `Sunshine: src/platform/windows/audio.cpp:607`, `mic_wasapi_t::init` 中的 `CreateEventA(nullptr, FALSE, FALSE, nullptr)`. 原手动复位事件没有 `ResetEvent`, 第一次通知后会使等待循环持续立即返回.
- [x] Windows COM guard 最后析构. 初始位置为 `src/audio/platform/windows.rs:381-388,466-474,1172-1175`. `_com` 已移到两个上下文结构体的末尾. Rust 按字段声明顺序析构, 因而所有 COM 接口先 `Release`, 然后才 `CoUninitialize`. 构造失败路径中的局部 guard 仍先创建, 后于接口局部变量析构. 上游资源包装参考 `Sunshine: src/platform/windows/audio.cpp:227-239,286-295`; 该错误来自 Rust 生命周期适配, 不是上游同名字段问题.
- [x] macOS 双向 SPSC ring 部分初始化失败可安全清理. `native/macos_audio_ring.h` 的 `ARAudioRing::initialized` 仅在数据区和 Mach semaphore 均创建成功后设为真. 信号量创建失败时释放数据区并返回原始状态, close/free 跳过未完成初始化的实例. 旧 ARFloatRing 和全部 pthread ring 辅助函数已删除. 上游清理边界参考 `Sunshine: src/platform/macos/av_audio.mm:467-505,540-552`.

### Windows endpoint 选择

`src/audio/platform/windows/endpoint.rs` 将选择策略与 COM 查询从流处理拆开. `CaptureConfig.device_name` 和 `PlaybackConfig.device_name` 在 Windows 上解释为不透明的 MMDevice endpoint ID, 不是友好显示名称. `None` 表示默认输出跟随; `Some(id)` 表示固定设备. 空字符串或内嵌 NUL 在创建线程/事件/COM 前返回 InvalidConfig; 合法 ID 保留空白和 Unicode, 转成带终止零的 UTF-16. 未新增配置字段或改变现有默认值.

默认选择调用 GetDefaultAudioEndpoint(eRender, eConsole), 固定选择调用 GetDevice. 两者都检查返回接口非空以及 GetState 为 ACTIVE, 查询失败或设备离线作为 Backend 错误, 不静默回退默认输出. 固定 ID 必须指向适合 loopback/render 的输出 endpoint; 不在这里枚举输入设备或匹配显示名称. 后续 WASAPI 初始化仍负责报告不支持的设备/格式.

对照 `Sunshine: src/platform/windows/audio.cpp:719-726,954-973,987-1008`, 捕获线程持有同一份选择策略跨越原生设备恢复, 固定选择不会因默认输出变化而重新选到别的设备. 只有默认模式保存已绑定默认 ID 并按原有周期轮询, 固定模式连默认 ID 查询都不执行, 因而默认设备消失也不影响固定选择. 播放沿用相同选择逻辑, 失败后仍由 runtime/render 销毁并重新打开输出和解码器. 不改变系统默认设备或主机音频路由.

9 项 endpoint 单元测试使用内存 COM vtable 验证实际选择函数, 包括 GetDevice/GetDefault 方法分流, Unicode/空白 ID 不失真, 默认轮询期限, 固定选择不查询默认输出, GetState 失败和非 ACTIVE 时接口释放, 成功返回空接口的防御以及固定 ID 离线/恢复仍保持原选择. 平台入口另测试非法 ID 在打开设备前拒绝. Windows 原生测试通过, 不创建真实声卡或读取设备配置.

与 Sunshine 仍有边界差异: 不实现 get_sink_device 的友好名称/多字段匹配和虚拟 sink 标识解析, 默认变化继续用轮询而非 IMMNotificationClient. 固定设备自身失效仍依赖 WASAPI 错误, 不宣称捕获事件暂时无数据就是设备停滞. 真实断开/重连, 默认切换和固定设备选择仍待硬件验证.

### Windows 多声道 PCM 格式

`src/audio/platform/windows/format.rs` 对照 `Sunshine: src/platform/windows/audio.cpp:60-130,326-397`, 为 2/6/8 声道生成 32-bit float WAVEFORMATEXTENSIBLE. 默认 speaker mask 分别为 0x3, 0x3f 和 0x63f, 有效位数 32, cbSize=22, SubFormat 为 KSDATAFORMAT_SUBTYPE_IEEE_FLOAT. WAVEFORMATEX 明确 packed(1) 为 18 字节, 扩展结构为 40 字节, valid bits/mask/GUID 偏移分别为 18/20/24. 不把 Rust 自然对齐的 20 字节 header 嵌入扩展格式.

流初始化先 GetMixFormat, 用 RAII 释放返回的 CoTaskMem 存储, 再 Initialize/SetEventHandle/查询实际缓冲和周期. 查询失败或成功返回空指针均阻止 Initialize. 仅在 native header 为 EXTENSIBLE 且 cbSize>=22 时读取 mask, 支持非对齐存储; 不读取短 header 后方的扩展字段. 相同声道数时允许标准 stereo/7.1 及 5.1 back/side 布局, 六声道环绕对位置保持不变. 与 Sunshine 不同, 不盲目采用任意同声道数的 height/wide/custom mask, 不兼容布局请求标准 mask 并交给 Windows AUTOCONVERTPCM 转换. 初始化失败仍返回设备错误, 不偷偷改成 stereo.

双向 backend 接受上述三种声道数, 使用相同的 interleaved f32 顺序 FL,FR,FC,LFE,BL,BR,SL,SR. 不做多余的 PCM 重排. 格式测试逐字节检查 ABI 和 GUID, 初始化替身检查 5.1 native side mask 实际传入捕获和播放的 Initialize, 队列测试覆盖三种布局和 5/10/20/40/60 ms 的完整帧与静音补尾. 真实 Opus 测试逐声道单独输入, 检查两个质量模式中解码输出的同索引能量和其它声道泄漏, 不仅检查输出非零.

这是底层格式能力, 默认会话仍为 stereo, 未增加 GUI/CLI 多声道选择或跨平台布局协商. macOS tap 仍限定 stereo, Android 能力边界不变. 默认 Opus mapping 已对齐固定 Sunshine 的顺序编码表, 并用不依赖 Synly 参数的原始 C API 做编码和交叉解码对照, 详见 [声道映射对照](audio-codec-mapping.md). 真实多声道设备的物理扬声器位置, 驱动格式支持及 Windows 混音转换仍待硬件验证.

### macOS ring 生命周期复核

| 路径 | 初始化和失败行为 | 清理前提 |
| --- | --- | --- |
| 捕获构造的声道, 系统版本, tap 或 converter 检查失败 | 对象已由 FFI 强引用持有, start 返回 false | 显式 shutdown 清理已创建资源, 只有全部成功才释放对象 |
| 任一方向 SPSC 数据区分配失败 | 返回错误, 未创建 Mach semaphore | close/free 跳过 |
| 任一方向 Mach semaphore 创建失败 | 释放数据区后返回原始状态 | close/free 跳过, 不释放不存在的信号量 |
| 捕获 SPSC ring 初始化成功, 后续 IOProc 创建或启动失败 | start 返回 false, FFI 进入统一 shutdown | close 后先注销属性监听, 再逐项检查 Stop/DestroyIOProcID/DestroyAggregate/DestroyTap; 失败立即隔离, 不释放 converter 和 ring |
| 捕获属性通知注册或注销失败 | 注册失败进入统一 shutdown, 部分成功的注册逐项回收 | 注销任一失败即隔离 owner 并禁止后续创建; 在途 block 仅强引用独立信号 |
| 播放 ring 初始化失败 | `ar_macos_playback_create` 检查非零状态码, 直接释放 engine | init 已自行回收部分资源 |
| `AudioQueueNewOutput` 失败 | 进入统一播放清理, 若返回非空 queue 也尝试 Dispose | 不存在 queue 或 Dispose 成功才释放 ring/engine |
| `AudioQueueAllocateBuffer`, enqueue 或 `AudioQueueStart` 失败 | close ring 后执行 `AudioQueueDispose(..., true)` | Dispose 失败保留整个 engine 和缓冲 |
| 正常播放销毁 | close ring, 注销默认输出通知, 从工作线程同步 Dispose | 全部成功后释放通知 owner 和 ring/engine, 不再单独调用 Stop |

捕获与播放分别检查 `ar_capture_ring_init` 和 `ar_playback_ring_init` 的状态码. 共用 `ar_audio_ring_close/free`, 均返回 void. FFI destroy 现在返回系统清理状态, Rust Drop 在失败时记录 error. 创建和销毁由专用生命周期 mutex 串行化, 正常读写及实时回调不取得此锁. `initialized` 不提供并发销毁保障; Rust 仍须保证读写与句柄销毁不并发.

SDK `AudioQueue.h` 的 AudioQueueDispose 契约规定: 从非回调线程调用同步 Dispose 后, 不再向应用交付回调. synly 仅在返回 noErr 时据此释放上下文. 捕获先注销属性监听, 再检查 Stop 和注销 IOProc, 随后依次销毁 aggregate 与 tap. 失败时不推断设备已停止, 不把错误码当成成功.

任一清理失败都会关闭对应 ring, 将仍持有原生资源的对象放入进程内隔离链并保留首次失败码. 捕获保留 FFI 强引用, 因而初始化失败后也不会因 ARC 析构而释放回调 context. 所有后续 macOS 音频创建在分配或系统 API 调用之前被拒绝, 已存在的其它句柄仍可关闭. 隔离对象数量不会因重试继续增长, 但可能包含首次故障前已创建的多个实例. 错误映射为 `BackendFatal`, 不属于捕获/播放重试范围; 若故障发生在 Drop 中, 至迟下一次打开识别永久失败. 必须重启应用恢复原生后端.

这是有意保留资源换取内存安全的保守策略, 不是自动回收或热恢复. 隔离资源会占用内存和原生句柄直到进程退出, 也不宣称失效驱动已经停止回调. 生产代码没有清除隔离状态或重用失败句柄的接口.

`native/tests/macos-audio-lifecycle-tests.m` 使用真实生产创建/销毁函数, 但所有设备 API 均替换为内存对象. 覆盖 AudioQueue 各缓冲分配/enqueue/start 失败与 Dispose 再失败, tap/aggregate/stream 查询/IOProc 各初始化失败, 捕获四个清理阶段逐项失败, 同步清理过程中及失败返回后的迟到回调, 多个既有句柄隔离, 每次故障后各 100 次创建拒绝, 以及清理失败与另一个创建线程竞争. 测试专用恢复替身后释放隔离对象, 检查分配归零; 这不是产品可用的恢复能力. ASan/UBSan 和 TSan 入口分别为 `just audio-native-test` 与 `just audio-ring-race-test`.

### macOS PCM 转换边界

`native/macos_audio_conversion.h` 对应 Sunshine `av_audio.mm:70-98,125-175` 的输入提供与转换流程, 并按 AudioConverter 输入回调契约修正暂缺数据处理. SDK 指出输入回调返回非零状态和 0 包可暂停转换, `FillComplexBuffer` 同时返回此前已产出的输出. 成功返回 0 包用于真正的流结束, 不能在每次设备回调末尾使用.

- 初始化查询实际输入 stream 的 `kAudioStreamPropertyVirtualFormat`, 校验 native packed Float32, 单个 buffer, 声道数, 每帧/每包字节数和有限采样率. 不再用设备 nominal rate 和第一个 buffer 的声道数构造猜测格式. 多 stream, planar 或整数 PCM 目前明确拒绝, 尚未实现格式转换支持.
- `ar_pcm_push` 将暂缺状态视为正常边界并保留已产出的样本, 输出块满时继续调用转换器. 必须等转换器再次请求输入后才结束设备回调, 避免下一次 IOProc 覆盖转换器仍借用的输入内存. 无进展或超过输出预算时返回错误, 不回退为原始格式数据.
- 预分配依据实际设备输入块, 输出容量属性, 采样率比和 prime 信息, 不再使用固定 `+32` 帧余量. 单次 IOProc 的接受上限为两个预分配输出块, ring 另加一完整读取帧. 这是明确的运行预算, 超限会停止链路, 不是对所有 Core Audio 版本最坏输出量的证明.
- NULL 数据但有合法帧数时提供等长零样本并走同一转换器. 空回调不制造一整编码帧, 以免改变时间线. 超过打开时的最大输入块, 非整帧字节数或声道变化均报告错误. 运行中格式/缓冲变化通过独立属性通知使旧实例失效, 再由工作线程重新查询, 详见属性变化小节.

`just audio-native-test` 同时运行故障注入和真实 AudioConverter 测试, 全部启用 ASan/UBSan. 真实测试将半秒输入分别按整块, 512 帧, 不等长块和强制每次仅输出 7 帧转换到 48 kHz 双声道, 比较完整帧数与逐样本结果. 已覆盖 8/44.1/48/96/192 kHz 双声道和 44.1 kHz 单声道, 每次输出 24,000 帧, 与整块参考的差异小于 0.00001. 每次提交后立即用 NaN 覆盖借出的输入, 验证后续转换不再引用它. 另验证格式拒绝, 静音帧数和两次缓冲分配失败的资源回收. 测试不创建 tap, 不验证真实设备的 stream 查询与切换.

### macOS 捕获 SPSC 边界

捕获使用公共 `ARAudioRing` 存储, 只允许一个 IOProc 生产者和一个读取线程. 数据区由读取/写入游标的 acquire/release 发布和回收, 两侧各自更新自己的游标. 游标按完整声道帧在两倍容量范围循环, 不要求容量为二的幂, 不因取整扩大时间预算. 一次输入最多执行两段 memcpy, 不分配, 不获取 pthread mutex, 不等待条件变量. 初始化检查所有使用的原子类型均 lock-free, 不支持时直接失败.

- 容量为 `max(30 ms, 协商帧 + 实际转换回调输出上界)`. 写入空间不足时整块拒绝并饱和累计丢弃样本. 这与 Sunshine TPCircularBuffer 拒绝超容量输入的所有权约束一致, 不再由生产者覆盖最旧数据. 不能把该策略表述为始终保留最新音频.
- 通知采用一个 Mach semaphore 和原子 notified 标志. 最多保留一个未消费通知, 连续成功读取不积累计数. 工作线程消费通知后以 acquire/release exchange 清标志, 再检查游标与关闭状态, 覆盖通知合并期间生产新数据的竞争. 等待使用单调截止时间, 无数据和重复通知都不能延长总预算.
- 关闭/失败先发布状态再通知, 保留首个故障码; 读取若与关闭重叠会拒绝提交该帧. 释放仍要求设备 IOProc 和读取方都已停止. capture context 为对象持有的 C 成员, 设备停止前保持存活. Core Audio 停止或注销失败时保留对象强引用并隔离, 详见生命周期复核.
- 与上游的双映射 TPCircularBuffer 和 dispatch semaphore 不同, synly 使用显式两段复制和 Mach semaphore. 这是 SPSC 行为适配, 不是原文件逐行复制. 去掉用户态互斥不等于证明 AudioConverter 或系统 semaphore_signal 的硬实时耗时上界.

`just audio-native-test` 增加 4 组 SPSC 测试, 包括内存/信号量初始化失败回收, 整块溢出拒绝, 非二幂游标回绕, 60 ms 拼帧, 通知合并, 超时, 数据/关闭/故障唤醒及 34 万帧双线程顺序验证. `just audio-ring-race-test` 对同一真实双线程测试启用 ThreadSanitizer. ASan/UBSan 和 TSan 均通过. 生产 IOProc 的无转换, 暂缺转换和失败路径还通过 mutex/calloc/free 注入守卫, 防止自身代码在回调中使用这些操作; 守卫不拦截系统框架内部调用.

### macOS 捕获无回调停滞

`native/macos_capture_health.h` 使用单调时间和 lock-free 原子心跳, 单独判定 IOProc 是否连续 5 秒没有执行. 启动尝试前建立初始期限, 因而首个回调一直不来时也能检测. 回调在格式转换前更新心跳, 不按样本音量或写入 ring 的样本数更新. 持续的空回调, 合法 NULL 数据对应的静音和暂缺完整编码帧均不会仅因没有 PCM 输出而触发重建.

读取线程在读取前后检查期限, 并将单次阻塞预算限制在剩余无回调时间内. 缓冲中残留旧帧不会延长健康窗口. 超时判定和回调心跳使用同一原子时间戳的 CAS: 心跳先成功则使用新的期限, 超时先成功则写入终止哨兵, 迟到回调不能重新激活该实例. IOProc 入口另有一次非等待的重入门控, 防止两个生产者同时操作 AudioConverter, ring 和健康状态. 回调不执行重建或日志格式化.

到期后发布 `AR_CAPTURE_STALLED`, 关闭 ring 并让 read 返回可恢复的 Backend 错误. 现有捕获工作线程负责停止/销毁旧对象, 退避 5 秒后重新打开; 编码器, UDP, RTP/FEC 和 AEAD 状态不变. 如果随后清理失败, 仍进入上一节的 BackendFatal 隔离, 不把清理失败当成可重试停滞.

对照 `Sunshine: src/platform/macos/microphone.mm:30-62`, 上游在等待不到数据 5 秒后写零并返回 timeout, 不在该路径重建. synly 借用其 5 秒时间尺度, 但新增无回调健康检查和可恢复错误, 不宣称这部分与上游逐行相同. 检测只由活跃读取线程驱动, 系统打开/停止 API 自身卡住仍不可强制中断. 若真实设备在正常静音时完全停止回调, 也可能触发该保守重建; 尚未实测其驱动行为. 默认设备/格式变化通知见下一节; 持续空回调却永不产出 PCM 的异常判定仍未覆盖.

`macos-capture-health-tests.c` 的 3 组测试通过注入时间验证首回调缺失, 5 秒精确边界, 1 ms 剩余等待预算, 持续心跳和时钟异常, 并进行 20,000 次真实线程心跳/终止 CAS 竞争. 生命周期测试使用真实生产 create/read/callback/destroy 和真实 AudioConverter, 仅替换设备 API 与时间: 验证长读取被期限限制, 连续空回调/静音不误重建, 停滞时拒绝旧积压, 迟到回调无法复活, 清理成功后可重新打开, 以及清理失败时保留隔离规则. ASan/UBSan 和 TSan 入口沿用两项原生测试 recipe. 生命周期测试另以真实单调时钟与 Mach semaphore 等待 5 秒无回调期限, 验证 60 秒读取请求不会一直等待; 设备 API 仍为替身, 不代表真实设备的超时与恢复已通过实测.

### macOS 捕获属性变化

`native/macos_audio_changes.h` 为两个方向共用的 HAL 通知模块, 捕获注册五项属性监听: 系统默认输出设备, aggregate 输入 stream 列表, aggregate 缓冲帧数, aggregate 存活状态和输入 stream 的 virtual format. 默认输出监听在创建 tap 前建立; aggregate 监听在写入请求参数后, 查询实际参数前建立; stream 格式监听在查询 ASBD 前建立. 初始化期间观察到变化会放弃该实例, 不清除通知后继续使用旧查询结果.

通知 block 只向独立信号对象的 lock-free 原子字段合并原因位, 不查询设备, 不拿生命周期锁, 不引用 ring, 不分配转换缓冲或执行重建. IOProc 入口和读取线程在读取前后检查信号, 发布 `AR_CAPTURE_CHANGED` 使旧实例失效, 已排入的旧 PCM 也不再作为成功帧返回. 没有后续 IOProc 时, 工作线程至迟在当前读取返回后检查; 正常运行时每次读取预算为 200 ms. 通知本身不唤醒 ring, 直接 FFI 调用者若使用更长预算仍受前一节的无回调期限限制. 原有 5 秒捕获退避及编码/UDP 状态保留策略不变.

SDK `AudioHardware.h:374-421` 规定 HAL 对 listener block 执行 Block_copy 并持有到匹配的 Remove, 但没有明确承诺 Remove 等待所有在途回调结束. 因此 block 只强引用 `ARAudioChangeSignal`, 不引用捕获 owner 或 converter/ring. 成功注销后, 即使系统已排队的 block 仍被交付, 它也只修改自己的信号. IOProc 使用的信号指针则由捕获 owner 持有, 直到 IOProc 清理成功后才释放. 没有循环强引用, 不依赖脆弱的裸指针注销屏障假设.

每个成功注册的监听都记录 object/address, 逆序注销且逐项检查状态. 注销失败会保留 owner 和剩余注册记录, 进入 BackendFatal 隔离, 其代价与前文的清理失败策略相同. 即使系统对象已消失导致 Remove 返回错误, 也不把错误猜成清理成功. 此路径优先保证上下文安全, 可能要求重启应用才能恢复音频.

`native/tests/macos-capture-change-cases.h` 由生命周期测试调用, 覆盖五类变化分别使旧 PCM 失效, 无关 selector 忽略, 读取等待期间变化, 注册期间变化, 五个 Add/Remove 故障点, 部分注册失败后注销再失败, 清理成功后新实例不继承旧信号. 另验证捕获和 ring 已释放后仍可调用被保留的 block, 最后一个 block 释放后信号对象也释放, 以及 20,000 次通知与销毁并发. 原回调测试的 mutex/calloc/free 守卫覆盖实际通知 block 和收到变化的 IOProc 路径. ASan/UBSan 和 TSan 均使用内存设备替身, 不打开真实音频设备.

这是 synly 的属性失效与生命周期扩展, 不是 Sunshine `av_audio.mm` 中现成监听实现的逐行复制. 默认输出通知只触发重建全局 tap, 不等于已支持指定 endpoint 或改变主机音频路由. 通知可能延迟或重复, 不能证明系统属性改变与最后一帧旧格式 PCM 之间存在同步边界. 实际切换, 默认设备消失及重复通知是否导致多余重建仍待设备实测. 播放端默认输出变化处理见下节.

### macOS 播放默认输出变化

播放使用同一 `ARAudioChanges` 模块, 但每个实例拥有独立信号和注册, 只监听系统默认输出设备. 在 AudioQueueNewOutput 前注册, 初始化最后检查是否已发生变化; AudioQueueStart 内或之后到达的变化由第一次回调或提交观察. 回调在消费软件队列前检测失效, 不继续 enqueue; 提交在等待前后检测, 错误为 `AR_PLAYBACK_CHANGED`, 经 Backend 错误进入已有 1 秒退避与设备/解码器重建. 捕获和播放收到同一个系统事件时分别失效, 关闭一个方向不会释放另一方向的通知信号.

C engine 使用显式 bridge-retained 的通知 owner 句柄, 实时回调只读其稳定的 C 状态指针. 关闭时先关闭 ring, 注销通知, 同步 Dispose AudioQueue, 最后才释放 owner 和 engine. 任一步失败保留整个 engine 并进入永久隔离. FFI create/destroy 具有 autoreleasepool, Rust 工作线程无需提供 Objective-C 环境. 在途通知仍只持有独立信号, 不延长 engine/ring 生命周期.

通知本身不唤醒阻塞提交. 正常 AudioQueue 回调观察变化会关闭 ring 并唤醒提交方; 没有回调时, 阻塞提交仍受原有最多 100 ms 预算限制并在返回前检查通知. 无网络数据且 AudioQueue 完全停止回调时, 不主动启动重建线程; 下一次提交会报告失效. 已经交给系统的缓冲不在软件队列控制范围内, 不声称通知一到就能撤回正在发声的数据. 该实现只覆盖默认输出切换, 不宣称覆盖 AudioQueue 的所有设备属性与服务重置.

`native/tests/macos-playback-change-cases.h` 验证回调/提交任一路径失效且不消费旧积压, 新实例使用新信号, 55 ms 水位下的阻塞提交在 100 ms 后发现变化, 注册/初始化期间变化, Start 内变化, Add/Remove 失败与初始化清理失败后的隔离. 另验证播放销毁后保留的 block 仍安全, 20,000 次通知与销毁竞争, 最后一个 block 释放后信号也释放, 以及双向实例相互独立. 所有设备 API 均为替身, ASan/UBSan 与 TSan 不构成真实设备切换的验证.

### macOS 播放 SPSC 边界

`native/macos_audio_ring.h` 统一两个方向的样本所有权, 生命周期及合并通知. `macos_capture_ring.h` 负责捕获容量/溢出和完整帧等待; `macos_playback_ring.h` 负责播放水位/背压和设备请求不足时补零. 不保留旧 pthread ring 或捕获覆盖写辅助函数.

播放只有一个阻塞提交线程, 并要求 AudioQueue 回调一次只有一个消费者. 回调入口的 lock-free 原子门控在重入时立即发布故障, 不等待, 不允许第二个消费者访问游标; 不依赖 SDK 文档中未明确的内部线程串行保证. 提交以 50 ms 为提交前水位, 空间额外容纳一整帧, 校验固定提交大小. 缺空间时仅提交线程等待, 使用单调截止时间并限制为 `min(timeout, 100 ms)`. 回调最多两段复制后补零, 只消费已有样本, 有实际消费时才发布空间通知. 关闭/首个回调故障唤醒提交方, 超时或关闭不继续排入新帧. 回调先检查系统缓冲容量, 拒绝超界写入.

4 组播放测试覆盖 5/60 ms 帧水位, 8 ms 调用方超时及 100 ms 上限, 部分消费和补零, 非二幂游标回绕, 10,000 次消费通知合并, 消费/关闭/故障唤醒阻塞提交, 40 万帧双线程有序传输. 使用真实线程和 Mach semaphore, ASan/UBSan 与 TSan 均验证通过. `macos-audio-tests.m` 另直接调用生产 AudioQueue 回调, 使用自身代码的 mutex/calloc/free 守卫覆盖补零, 正常样本, 小缓冲拒绝与 enqueue 失败.

这是 Moonlight 播放水位语义与 AudioQueue 的适配, 不是 SDL renderer 的逐行移植. AudioQueue 内部排队缓冲仍不包含在软件水位中, 也未证明框架 API 的硬实时上界. Dispose 失败后的上下文隔离见生命周期复核, 不将隔离表述为成功停止设备.

## 待完成问题与上游映射

### P1: 延迟和恢复行为

- [x] 以时间预算约束原生软件缓冲. 捕获以 30 ms 为基准, 并保留一完整协商帧加实际设备包的拼帧空间, 避免 40/60 ms 帧在回调块长不整除帧长时正常丢样. Windows 依据 WASAPI 实际 buffer frames, macOS 依据实际设备 buffer frames 的转换输出上界. 播放容量为 50 ms 加一完整帧. Windows 捕获溢出按完整声道帧保留最新数据; macOS SPSC 容量不足时拒绝新输出块, 不覆盖消费者所有的数据. 两者均累计丢弃样本和最高水位, 关闭时输出 debug 统计. 这对应 Sunshine 按设备包与帧长分配缓存的意图; 30 ms 不是所有格式和设备下的容量硬上限.
- [x] Windows shared event 初始化请求默认缓冲. 捕获与播放均向 `Initialize` 传入 0/0 时长, 随后绑定事件, 查询实际 buffer frames, device period 和 stream latency 并记录. `windows/stream.rs` 统一处理, 不将请求值当作实际延迟. 尚未移植 SDL renderer 的设备缓冲选择.
- [x] 适配播放软件队列背压. 对照 `Moonlight Qt: app/streaming/audio/renderers/sdlaud.cpp:103-125`, 网络积压超过 30 ms 时先丢 PCM, 原生提交前软件队列超过 50 ms 时等待消费. 上游先按整包向下取整, 其开始等待边界随包时长不同, 不是精确的 50 ms; 原始 C++ 可执行证据见 [renderer 行为对照](audio-renderer-oracle.md). Windows 条件变量与 macOS Mach 通知替代 1 ms 轮询, 总等待不超过调用方 timeout 和 100 ms 中较小者. 50 ms 水位允许再接收一整帧, 故默认 5 ms 帧最大软件积压为 55 ms, 60 ms 帧为 110 ms. 超时返回错误, 与上游等待结束仍入队不同, 保持 synly 缓冲有界. 此水位不包括 WASAPI/AudioQueue 已提交设备缓冲, 不能当作端到端延迟上限.
- [x] Windows 捕获原生 ring 在重建边界丢弃旧音频. `begin_recovery` 清空 ring 并提高 generation, 恢复期间拒收, `finish_recovery` 再清空并允许新提交. 队列原语还验证旧代次提交即使在恢复后醒来也不会混入新样本. 播放端现直接销毁旧 ring, 不再在同一原生线程内重建.
- [x] 播放线程设备恢复与丢帧窗口. `runtime/render.rs` 对照 `Moonlight Qt: app/streaming/audio/audio.cpp:225-253`, 播放 Backend/Io 失败后销毁旧设备和解码器, 等待 1 秒再打开, 初始无设备同样重试. 成功后清理重建期间积压, 再按本次打开耗时设置丢帧窗口, 之后新建 Opus 解码器. 重试按单调时钟而非上游 200 包计数, 网络停流时仍可恢复; 配置, 编解码和不支持平台错误直接退出. UDP, RTP/FEC, 来源绑定及 AEAD 重放状态不重启.
- [x] 捕获失败后只重建设备, 保留发送状态. `runtime/capture.rs` 对照 `Sunshine: src/audio.cpp:248-272`, 先释放失败的输入对象, 再重试创建. synly 对首次无设备和读取 Backend/Io 错误都退避 5 秒, 不复制上游首次失败后只等待停止的行为, 也避免读取立即失败时反复打开. 超时不重建, 部分失败帧不入队; 重试等待可被取消. 编码器, RTP/FEC 和 AEAD 计数器不重新创建, 已完整采集的有界队列仍由编码线程处理. Windows 原生捕获的代次恢复继续保留.
- [x] 永久能力错误不进入设备重试. macOS 未实现的指定设备功能返回 UnsupportedPlatform, Windows 空或含 NUL 的 endpoint ID 返回 InvalidConfig, 合法但离线的指定设备返回可恢复 Backend. macOS 在进入 tap 创建前检查系统至少为 14.2. 该版本要求来自 SDK 的 AudioHardwareCreateProcessTap 声明. 不把配置错误当作设备暂时断开.
- [x] macOS 捕获无回调停滞检测. IOProc 单调时钟心跳连续 5 秒未更新则向工作线程报告可恢复错误, 不依赖 PCM 非零或普通读取超时次数. 详见捕获无回调停滞小节.
- [x] macOS 捕获属性变化通知接入设备恢复. 输入 stream/格式/缓冲/存活状态及系统默认输出变化使旧实例失效, 重新查询与清理仍在工作线程中, 详见属性变化小节.
- [x] macOS 播放默认输出变化接入已有输出恢复, 独立通知信号与捕获共用生命周期实现, 详见播放默认输出变化小节.
- [ ] 设备变化后的真实恢复验证. 当前无回调检测和属性监听不等于验证真实静音/断开/切换行为, Windows 停滞与正常无数据的区分仍须单独验证.
- [x] macOS 回调故障交付给工作线程. 转换或 `AudioQueueEnqueueBuffer` 失败时, ring 保存首个故障码, 关闭队列并唤醒等待读写. 两个方向的 SPSC ring 均只传递原始状态码. 错误字符串在读写线程格式化; Rust 捕获和播放工作线程分别执行设备重建, 不在实时回调中创建或销毁设备. 转换成功但没有输出时不人为插入一帧静音. 初始化阶段的 enqueue 返回值也会检查. 已用 `just audio-native-test` 注入转换和 enqueue 失败, 在 ASan/UBSan 下通过.
- [x] 播放恢复控制不进入实时回调. 原生失败回传到 `runtime/render.rs` 后释放旧输出, 在独立阻塞工作线程重建 WASAPI/AudioQueue. `FrameQueue::discard_until` 用条件变量持续清理待解码包, 无网络输入时仍到期, 监督器关闭队列可唤醒重试和恢复窗口. 系统打开/关闭 API 本身若阻塞, 仍须等待它返回, 不宣称可强制中断设备调用.
- [ ] 真实设备失效, 拔插及默认设备变化后的双平台恢复验证. 现有测试使用真实 UDP, Opus 和线程调度, 但设备打开/提交是可控替身.

### P2: 实时安全与可观测性

- [x] macOS 捕获回调采用预分配 SPSC 缓冲. `native/macos_capture_ring.h` 对照 `Sunshine: src/platform/macos/av_audio.mm:114-207` 的 TPCircularBuffer 生产/消费所有权实现, 不再共享 pthread mutex, 不逐样本加锁复制. IOProc 的 context 是纯 C `ARCaptureState`, 不再桥接为 ARC 强引用对象. 通知改用 Mach semaphore, 读取工作线程负责等待和格式化错误. 详见下文捕获 SPSC 边界.
- [x] macOS AudioQueue 播放回调改用公共 SPSC 存储, 不再获取 pthread mutex. 仅提交工作线程等待空间, 回调不足样本时补零并通知消费进展. 双方向回调自身的分配/互斥守卫及真实 SPSC 竞态验证通过. Windows 静音包已直接写零, 不逐包分配临时 Vec.
- [x] 修复 macOS 错误字符串的数据竞争. 创建和读写失败信息使用调用线程本地缓冲, `ar_macos_copy_error` 有界复制到 Rust 提供的存储; 回调错误通过实例 ring 的 OSStatus 传递. 不再返回共享可变裸指针. 故障测试验证线程间错误隔离, NUL 终止, 复制边界和读写线程获得回调故障码.
- [x] Windows 捕获和播放线程注册 MMCSS. `windows/scheduling.rs` 以线程级 RAII 注册 `Pro Audio`, 失败时记录警告并继续普通优先级, 退出时撤销; guard 不跨线程移动. 对应 Sunshine `mic_wasapi_t` 的注册/撤销路径, 已覆盖成功和失败的模拟测试.
- [ ] 补齐持续排队时长和恢复耗时观测. 两平台已在关闭时报告 ring 丢弃样本和最高水位, Windows 还累计恢复丢弃和背压超时样本. `windows/diagnostics.rs` 的 `DATA_DISCONTINUITY` 首次后最多每 5 秒输出一次, 跨重建保留. 连续时长观测和完整恢复耗时仍未覆盖.

### 功能对齐边界

- [x] Windows 固定 endpoint ID 与默认跟随分流, 见上文 Windows endpoint 选择.
- [ ] Windows sink 友好名称/虚拟 sink 解析与上游通知机制. 当前已在 capture_loop/playback_loop 中轮询默认输出 ID, 发现变化或查询失败会 Restart, 并非完全没有默认设备跟随. 固定 endpoint 不执行默认轮询. `Sunshine: src/platform/windows/audio.cpp:630,719-726,757-768,954-973,987-1018` 还使用通知回调和多字段匹配. 上游 `_fill_buffer` 的 WAIT_TIMEOUT 返回 timeout, `sample` 仅在 continuous_audio 模式下填零; 不能仅凭捕获事件暂时不来就认定驱动停滞并反复重建.
- [x] Windows 底层 2/6/8 声道 WAVEFORMATEXTENSIBLE, 已验证 ABI, native 5.1 mask, 队列完整帧及 Opus 声道索引, 见上文 Windows 多声道 PCM 格式.
- [x] 默认 Opus 编码映射与固定上游对齐, 参数独立的直接 C API 对照已复现并修正旧环绕声映射偏差, 见 [声道映射对照](audio-codec-mapping.md).
- [ ] 多声道会话选择/协商及自定义 surround 参数. `Moonlight Qt: app/streaming/audio/renderers/renderer.h:18-26` 与 `app/streaming/audio/audio.cpp:69-80` 定义解码映射边界. 编码表对齐不等于 RTSP/GameStream 互通或真实扬声器布局验证完成.
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
- [x] macOS 公共 SPSC 数据区和 Mach semaphore 初始化失败时释放已创建资源; 实际生产实现的故障注入测试通过 ASan/UBSan. 已删除旧 pthread 初始化实现及对应的过时测试.
- [x] 捕获和播放清理失败时保留回调上下文, 进入不可重试的隔离状态, 不在 Stop/Dispose 返回错误后释放内存. 故障注入验证迟到回调及初始化失败路径; 真实设备/驱动行为仍待实测.
- [x] 44.1 kHz 到 48 kHz 等真实转换的分块连续性, 暂缺与静音保持帧数约定.
- [ ] 真实设备不连续, stream 查询与格式切换后的 PCM 约定.
- [ ] 切默认设备, 拔设备和恢复设备后能够恢复, 且不播放旧队列.
- [ ] 持续运行和突发网络积压时, 排队时长可观测且不会无界增长.
- [ ] 启用 5.1/7.1 后以逐通道脉冲验证编码映射与播放布局.
