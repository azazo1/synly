# 原始 SDL renderer 行为对照

## 被执行的源码

参考版本为 Moonlight Qt `49bf1e80da945fc95547d8d64b40d54cbb2f3cb3`. `native/tests/build-audio-renderer-tests.sh` 直接编译该仓库的 `app/streaming/audio/renderers/sdlaud.cpp`, 使用其原始 `sdl.h` 和 `renderer.h`, 不复制或重写 renderer 实现. 三个输入文件已与固定提交逐字比较一致.

SDL, QtGlobal 和 Moonlight 队列查询由 `native/tests/audio-renderer-shim/` 声明和 `native/tests/audio-renderer-tests.cpp` 内存替身提供. 替身只记录 API 调用, 模拟设备状态, 排队字节和虚拟延时, 不链接 SDL/Qt 库, 不打开声卡, 不安装依赖, 不运行应用. 这验证原始 C++ 控制流, 不验证真实 SDL ABI, 驱动调度或声音输出.

复现入口需要已有的上游源码和 clang++:

```shell
just audio-renderer-test /path/to/moonlight-qt
```

该 recipe 在 Unix 上编译并运行 ASan/UBSan 测试, 编译使用 -Wall/-Wextra/-Werror. 当前已在 macOS 验证; 不据此宣称 Windows 或 Linux 上真实 SDL 输出已验证.

## 六组证据

| 组 | 实际执行和检查 |
| --- | --- |
| 格式与生命周期 | 2/6/8 声道乘以 5/10/20/40/60 ms 共 15 种请求; 原始 `want.samples=max(480,3*samplesPerFrame)`, float/native-endian, 默认播放设备, allowed_changes=0; 另用 120 样本覆盖 480 下限. 检查一帧分配, 完整及较短 PCM 字节提交, Pause/Close/free/Quit 顺序 |
| 网络积压 | 0 字节不查询队列; pending=30 ms 仍提交, 31 ms 开始丢弃; 积压丢弃发生在设备状态查询前 |
| 软件播放水位 | 三种布局和五种时长, 在向下取整后下一包边界的前一个完整 PCM 采样帧及边界本身共 30 种情况; 虚拟消费发生后才结束等待 |
| 等待耗尽 | 持续高积压下执行 100 次 1 ms 虚拟延时, 随后仍调用 QueueAudio, 返回成功 |
| 设备状态 | STOPPED 立即失败, 第二次等待后停止会失败, PAUSED 不等同 STOPPED; 第 100 次延时后停止不会再查询状态, 会进入 QueueAudio |
| 失败路径 | OpenAudioDevice 失败, PCM 分配失败后销毁已有设备; QueueAudio 返回负数时仅记录错误, submitAudio 仍返回 true |

120 样本分支只用于覆盖原始 renderer 的下限逻辑, 不表示 Synly 增加了 2.5 ms 会话时长. 子系统初始化成功是本替身的前提, 尚未测试真实 SDL_InitSubSystem 失败, SDL 的内部线程或全局子系统引用计数行为.

## 可选 SDL2 产品后端

工程现在提供 `sdl2-audio` feature. 启用后, `platform::open_output` 选择 `src/audio/sdl2.rs` 的 raw SDL2 FFI; 默认 feature 仍使用 WASAPI/AudioQueue. 该实现复用了原始 renderer 的 float32, `max(480, 3 * samplesPerFrame)`, queued-audio 水位和 stopped-device 检查, 并按上游在 SDL 入队失败时记录错误后返回成功.

SDL2 打开入口在初始化子系统前验证 PCM 请求: Opus 支持的 8/12/16/24/48 kHz, 2/6/8 声道和 5/10/20/40/60 ms. 一帧内存也在打开设备前准备. 空或含 NUL 的设备标识返回 InvalidConfig; 其他指定设备标识返回 UnsupportedPlatform. 平台设备标识并不等同 SDL 显示名称, 当前仅支持默认输出, 不会静默丢弃指定设备要求. `validation_tests.rs` 覆盖 75 个合法参数组合, 并通过平台入口验证非法参数的提前拒绝, 不打开声卡.

SDL2 不在 Cargo.lock 中, 也没有被自动安装. 启用 feature 时需要目标系统提供 SDL2 library. 已完成本机链接验证, 真实设备播放仍未验证.

`src/audio/sdl2.rs` 还包含不调用 SDL C API 的布局测试, 覆盖 stereo, 5.1, 7.1 的 interleaved f32 帧字节数和 5/20 ms 的 requested samples. 这验证 `StreamParams` 的样本数与声道数关系, 但不能代替 SDL2 library 链接或真实设备测试.

`src/audio/sdl2/lifecycle.rs` 将子系统初始化, 引用发布和最后一次退出放在同一互斥锁内. 初始化失败不增加引用, 最后一次退出完成前不能开始新初始化. 锁不进入 PCM 提交路径, 只约束输出对象的创建与销毁. 生命周期回归使用内存替身, 检查失败重试, 多引用释放顺序, Init/Quit 持锁边界和 8 线程共 8,000 次引用操作. 这不验证外部组件直接调用 SDL 的生命周期, 也不替代驱动测试.

SDL2 提交的 `src/audio/sdl2/backpressure.rs` 对照上游 `sdlaud.cpp:112-125`: 最多 100 次先检查 STOPPED, 再按整包取整比较 50 ms 水位, 超水位调用 `SDL_Delay(1)`. 耗尽 100 次后仍入队, 不追加状态检查. 空帧直接成功; SDL_QueueAudio 负返回值记录错误, 仍向上层返回成功, 避免与上游不同的设备重建. SDL 分支使用固定轮询次数, 不采用 AudioOutput 的调用方 timeout. 系统调度可延长实际等待, 100 次不是硬实时 100 ms 保证.

内存测试覆盖三种布局和五种时长的 30 个水位边界, 等待中消费, 耗尽后继续以及第 0/2/99/100 次等待后的停止状态边界. 最后一个边界与原始循环一致, 第 100 次等待后才停止不会由该次提交检测到. 原生后端仍保留自己的超时错误语义.

`src/audio/sdl2/submission.rs` 由实际 SdlOutput 调用, 将设备查询/延时/入队作为闭包边界. 故障注入测试验证入队错误后本次仍成功且下帧可提交, STOPPED 查询错误会阻止复制和入队, 空帧/不完整帧不会调用设备, 100 次等待耗尽后仅入队一次. 测试不依赖错误文案, 不故意破坏真实设备; SDL 错误码到 Rust 错误的 FFI 包装仍由源码对照, dummy 路径验证成功入队.

音频 runtime 提供 `bind_and_spawn_receiver_with_config` 和 `spawn_sender_with_config`, 让上层会话传入 `CodecConfig` 选择 stereo, 5.1 或 7.1. `AudioUdpReady` 携带 `AudioLayout`, 由接收端声明布局, 发送端按声明编码. 这不是探测两端声卡能力并自动选择布局. 两个公开入口在绑定 UDP 和启动任务前同步调用 `CodecConfig::stream_params`, 将已解析参数交给后台流水线. 无效帧时长不会先返回端口或任务句柄再异步失败. `runtime/channel_tests.rs` 在没有 Tokio runtime 的环境下验证两个方向的发送/接收入口拒绝无效时长; 设备可用性和实际 codec 初始化失败仍由工作线程报告.

本机通过 `pkg-config` 检测到 SDL2 2.32.70. 新增的 `just audio-sdl2-test` 会先要求 `pkg-config --exists sdl2`, 再由 `build.rs` 在 `sdl2-audio` feature 下解析 SDL2 的 `-L` 和 `-l` flags. 该 recipe 已完成真实 SDL2 链接, 运行布局测试和 feature Clippy. 测试没有调用 `SdlOutput::open`, 所以没有打开真实音频设备.


独立入口 `just audio-sdl2-dummy-test` 设置 `SDL_AUDIODRIVER=dummy`, 精确选择一个默认忽略的测试并串行执行. 测试先检查实际驱动名称, 再调用产品打开/提交/关闭代码, 已通过 stereo/5.1/7.1 与五种帧时长的 15 个组合. 暂停设备后检查帧字节数, 空帧不增加队列, 超水位耗尽轮询仍追加一帧, 恢复设备后检查消费和重新提交, 最后确认嵌套引用释放与子系统重新初始化. 它使用真实 SDL 库和 dummy 消费线程, 不访问物理声卡, 不验证音质或扬声器映射.

`RuntimeConfig.audio_layout` 持久化并沿 `RuntimeOptions` 和 `SyncSessionOptions` 传到 capability refresh. 接收端用该布局启动 codec, 并在 `AudioUdpReady` 中声明; 发送端按声明选择 codec. 旧配置和旧 `AudioUdpReady` 缺少布局时均默认为 stereo. GUI 的 "接收声道" 提供立体声, 5.1 和 7.1, 选择后立即发送保存命令, 下次连接生效. 当前通道继续使用建立时的布局, 不在运行中改动 codec. 此设置不把立体声源自动变成真实环绕声.

## 50 ms 水位的取整差异

上游 `sdlaud.cpp:120` 先按整包取整再比较 50 ms 水位, 因而边界随包时长变化:


| 包时长 | 上游开始等待的积压 | Synly 原生软件 ring |
| --- | --- | --- |
| 5 ms | >=55 ms | >50 ms |
| 10 ms | >=60 ms | >50 ms |
| 20 ms | >=60 ms | >50 ms |
| 40 ms | >=80 ms | >50 ms |
| 60 ms | >=60 ms | >50 ms |

这里只比较提交前的软件队列, 不包括声卡/系统音频引擎缓冲. Synly 的容量在 50 ms 水位之外预留一完整协商帧, 不是总播放延迟上限为 50 ms.

## 原生后端逐函数映射

| Moonlight 原始位置 | Synly 对应 | 保留或差异 |
| --- | --- | --- |
| 构造/析构 `5-17,75-89` | 平台输入/输出对象和 runtime/render 生命周期 | 原生后端无 SDL 全局子系统, 不能视为这些调用已逐行移植; macOS 清理失败还会隔离资源 |
| `prepareForPlayback:19-73` | WASAPI shared event 和 AudioQueue 初始化 | f32/采样率/声道参数保留; 原生系统缓冲由各自 API 决定, 不等同 SDL 的三包请求. 上游请求与实际 have.samples 也可能不同 |
| `getAudioBuffer:91-94` | runtime/render 的一帧 Vec | 一帧 PCM 存储, Rust 所有权代替 SDL_malloc/free; Synly submit_frame 要求完整协商帧 |
| `submitAudio:98-107` | runtime/render 先解码后按网络积压丢 PCM | 30 ms 条件保留, 不因过期 PCM 丢弃而停止推进解码器 |
| `submitAudio:112-125` | Windows 条件变量 / macOS 信号等待 | Synly 精确样本水位, 最长等待 100 ms 后返回错误, 不复制取整和耗尽后继续入队; 时间等待也不是上游 100 次轮询的逐行替换 |
| `submitAudio:115-117` | 原生设备失效错误及默认设备变化检测 | 都触发设备恢复, 但不是 SDL_GetAudioDeviceStatus 的同一状态模型 |
| `submitAudio:127-133` | 原生 submit_frame 错误进入 runtime/render 重建 | Synly 不把入队失败当作成功, 没有复制上游只记录错误的处理 |
| `getAudioBufferFormat:136-139` | 平台 interleaved f32 | 格式一致, 不证明驱动格式转换行为一致 |

上述表格只描述默认原生后端. 可选 SDL2 路径采用 Rust FFI 移植, 没有将原始 C++ 类直接编译进产品. 它已复制 float32/三包请求/整包水位/100 次轮询/入队失败仅记录的规则, 网络积压判断由 `runtime/render.rs` 调用 `sdl_policy.rs` 完成. 生命周期为支持多个输出而使用共享引用管理, 参数预校验和完整非空帧约束也是 Synly 的边界. 真实设备切换, 上层重建时序和端到端延迟仍不能由内存替身或 dummy 驱动证明.
