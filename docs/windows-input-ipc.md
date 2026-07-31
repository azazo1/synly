# Windows 输入代理 IPC 修复

## 问题根因

Windows 输入代理原先在 Tokio named pipe 上进行持续双向读写. 在高频光标同步和反复跨屏后, pipe 的某一方向可能停止推进, 导致请求已经由 GUI 写出, 但响应无法按时返回. 后续看到的 `response timeout`, agent 断开和输入能力关闭都是传输停滞的结果, 不是 Win32 鼠标注入本身失败.

## 核心修复

- command pipe 仅负责 `GUI -> agent` 请求.
- event pipe 仅负责 `agent -> GUI` 响应和输入事件.
- 两条 pipe 均由固定的专用线程独占. 创建, 连接, 读写, 取消和关闭都在 owner 线程内完成.
- 传输直接使用 `CreateNamedPipeW`, `CreateFileW`, `ReadFile` 和 `WriteFile`.
- 阻塞 I/O 使用 OVERLAPPED event 等待, 超时和关闭使用 `CancelIoEx`.
- 可靠请求采用有界队列, 每次只允许一个请求在途, 完整写入后只启动一个响应期限.
- cursor 和 Motion 使用 latest-value 合并, button 和 wheel 之前先写出最新 cursor.
- agent backend 不再直接接触 pipe, 只通过内存队列与 command reader 和 event writer 交互.

## 行为保证

- 已提权 agent 会跨 `Start/Stop` 和设备重连复用, 不会每次控制都重新弹出 UAC.
- 当前 Synly 主进程已提升时, agent 会直接继承当前令牌启动, 不请求 UAC.
- 未申请提权时仍可使用普通权限基础输入控制.
- 已启用提权 receiver 后发生 IPC 故障会直接报告失败, 不会在当前会话中静默降级到普通权限.
- pipe DACL, token, PID, session 和二进制路径校验继续保留.
- agent 日志独立写入本机滚动文件, 不经过 event pipe. 日志目录为 `%LOCALAPPDATA%\synly\logs\input-agent`.

## 验证

- Windows 真实 named pipe 压力测试覆盖 20000 次 cursor 更新和 1000 轮进入, 返回, 再次进入.
- 压力结束后继续验证 button 和 wheel, 并覆盖可靠 Event 和合并 Motion.
- 覆盖半帧, 错误长度, read timeout, response timeout 和 peer 退出.
- Windows `cargo test --all-features`: 194 passed, 0 failed, 1 ignored.
- Windows `cargo clippy --all-targets --all-features`: 通过, 无警告.
- macOS `cargo test --all-features`: 186 passed, 0 failed.
- macOS `cargo clippy --all-targets --all-features`: 通过, 无警告.
- macOS 到 Windows 的真实控制链路已确认可正常跨屏, 点击和持续控制.
