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

## 50 ms 水位的取整差异

上游 `sdlaud.cpp:120` 使用 `queuedBytes / frameBytes * frameDurationMs <= 50`. 除法先取整, 因而开始等待的实际积压边界随包时长变化:

| 包时长 | 上游开始等待的积压 | Synly 原生软件 ring |
| --- | --- | --- |
| 5 ms | >=55 ms | >50 ms |
| 10 ms | >=60 ms | >50 ms |
| 20 ms | >=60 ms | >50 ms |
| 40 ms | >=80 ms | >50 ms |
| 60 ms | >=60 ms | >50 ms |

这里只比较提交前的软件队列, 不包括声卡/系统音频引擎缓冲. Synly 的容量在 50 ms 水位之外预留一完整协商帧, 不是总播放延迟上限为 50 ms.

## 当前逐函数映射

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

以上对照证明了保留规则和差异的实际控制流, **没有把原始 SDL renderer 接入 Synly 的产品播放路径**. Synly 另外将可移植的网络积压判断集中在 `src/audio/runtime/sdl_policy.rs`, 由 `runtime/render.rs` 使用. 这只复用 SDL renderer 的 30 ms 网络丢弃规则, 没有伪装成 SDL API, 也没有改变原生后端的精确水位和错误语义. 当前 WASAPI/AudioQueue 仍是行为适配. 实现真正 SDL renderer, 或对继续采用原生后端的目标边界作出明确选择, 仍是整体移植目标的未完成项. SDL 生命周期, 真实设备切换, 上层 renderer 重建时序和端到端延迟不能由本替身测试代替.
