# Synly

Synly 是一个面向局域网的跨平台同步应用. 默认启动 Slint GUI, 可在系统托盘后台运行, 并在当前连接中动态调整剪贴板, 音频和输入方向. 文件工作区或角色等会影响会话边界的设置会自动安全断开并重连.

Synly 支持 Windows, macOS 和 Linux. 文件与剪贴板同步可在三大平台使用. 音频运行时目前支持 Windows 和 macOS. 输入同步目前支持 Windows 和 macOS.

## 主要功能

- Slint 主窗口和原生系统托盘.
- 状态, 设备, 同步, 安全, 设置, 日志和 About 页面.
- mDNS 与 LND 设备发现聚合.
- 未信任设备使用 bootstrap 指纹, SPAKE2 PIN 和临时 mTLS 完成配对.
- 可信设备使用身份公钥和长期 mTLS 免 PIN 重连.
- 文件同步支持 off, send, receive, both 和 auto.
- 剪贴板同步支持文本, RTF, HTML, 图片和受大小限制的文件.
- 音频支持单向 send 和 receive.
- 输入支持单向 send 和 receive, 包含边缘切换和紧急收回热键.
- 剪贴板, 音频和输入模式使用 protocol 17 capability generation 热协商.
- 文件扫描间隔和删除策略可在当前会话中更新.
- 角色, 对侧, 工作区和监听端口变化会自动重建会话.
- 单实例运行. 重复启动会激活已有窗口.
- 支持关闭到托盘, 启动隐藏, 恢复上次会话和登录启动.
- 隐藏窗口收到配对请求时显示不含 PIN 的系统通知, 点击后恢复对应 modal.
- 窗口尺寸会持久化并在下次启动时恢复.
- tracing 同时输出到终端, 每日滚动日志文件和 GUI 环形日志缓冲区, 日志级别可实时调整.

## 安全顺序

未信任设备在 PIN 之前只交换一次性 bootstrap 公钥. 双方核对 bootstrap 指纹和会话 randomart 后, 使用 PIN 完成 SPAKE2. 设备身份, 工作区摘要和能力信息只在临时 mTLS 建立后传输.

固定 PIN 只保存在当前进程内存中. PIN, 密钥, PAKE 数据和剪贴板内容不会写入 tracing 日志.

可信设备可保存对侧身份公钥和 TLS 根证书. 撤销当前对侧信任会立即断开连接, 后续可信重连会被拒绝.

## 构建

Rust 版本需要满足 Slint 1.17.1 的要求. 当前开发基线使用 Rust 1.97.1.

```shell
cargo build --release
```

开发运行:

```shell
cargo run --
```

发布产物位于 `target/release/synly`.

### Windows

推荐使用 MSVC 工具链和 Windows 10/11 SDK. 音频依赖 Opus.

```powershell
rustup default stable-x86_64-pc-windows-msvc
vcpkg install opus:x64-windows-static
$env:VCPKG_ROOT="C:\path\to\vcpkg"
cargo build --release
```

Windows 主 GUI 使用 `asInvoker` manifest 以普通权限运行. 需要管理员输入能力时, GUI 通过 `ShellExecuteW("runas")` 启动当前 `synly.exe` 的隐藏输入代理子进程, 并使用命名管道完成握手. 主进程与子进程来自同一个构建产物, 避免 IPC 协议版本错配. 登录启动且窗口隐藏时不会主动弹出 UAC, 需要用户从托盘或设置页确认授权.

GUI 和提权子进程通过随机命名管道与随机 token 通信. 管道 DACL 只允许当前用户和 SYSTEM, 双方校验 IPC 版本, PID, session ID, 映像路径和安装目录. release 构建要求当前可执行文件通过 Authenticode 校验, debug 构建会记录签名校验警告但允许本地未签名产物. 管道断开, 心跳超时, 子进程退出或进入非 `Default` 输入桌面时会立即释放输入状态.

### macOS

macOS 音频采集要求 macOS 14.0 或更高版本.

```shell
xcode-select --install
brew install pkg-config opus
rustup default stable
cargo build --release
```

### Linux

Debian 或 Ubuntu:

```shell
sudo apt update
sudo apt install -y build-essential pkg-config libopus-dev
rustup default stable
cargo build --release
```

Fedora:

```shell
sudo dnf install -y gcc gcc-c++ make pkgconf-pkg-config opus-devel
rustup default stable
cargo build --release
```

Linux 音频和输入运行时目前不可用, 但文件与剪贴板同步不受影响.

## GUI 使用

直接运行 `synly` 启动 GUI.

```shell
synly
```

首次启动总是显示主窗口. 关闭窗口默认隐藏到托盘. 托盘菜单可以打开窗口, 连接或断开, 快速切换剪贴板, 音频和输入, 以及退出应用.

安全页可以设置仅在当前进程内有效的固定 PIN. 留空时 host 为每次未信任配对生成随机 PIN. 固定 PIN 不写入配置, 状态快照或日志.

设备页持续显示发现结果, 协议版本, 来源, 信任状态, 地址和广播能力. protocol version 不兼容的设备无法连接.

剪贴板, 音频和输入开关会立即发送 capability update. 开启新能力时, 本机等待对侧 ack 后再启动任务. 关闭能力时, 本地任务会立即停止. ack 超过 5 秒未返回时, 当前会话会受控重连.

文件模式, 路径, 初始来源和最大目录深度需要重建文件会话. GUI 会保留进程和托盘, 只重连当前对侧.

设置页可以调整设备名, mDNS/LND, 剪贴板载荷限制, 缓存目录, 传输帧限制和后台行为. capability 变化会重新发布发现元数据. 设备名, 发现后端或传输限制变化使用受控重连, 剪贴板限制从下一项载荷开始生效.

启用删除同步前会显示确认对话框. 确认后更新当前策略, 并请求对侧强制发布一次文件快照.

## Headless

只有显式传入 `--headless` 才会进入无界面模式. Headless 不进行任何终端询问. 参数不足时直接失败.

Host 示例:

```shell
synly --headless --host --fs auto --initial this --pin 123456 --accept --trust-device .
```

Join 示例:

```shell
synly --headless --join --peer workstation --fs auto --initial other --pin 123456 --trust-device .
```

后续仅允许可信设备:

```shell
synly --headless --host --fs auto --initial this --trusted-only .
synly --headless --join --peer workstation --fs auto --initial other --trusted-only .
```

只同步剪贴板:

```shell
synly --headless --host --fs off --clipboard both --pin 123456 --accept
synly --headless --join --peer workstation --fs off --clipboard both --pin 123456
```

音频发送和接收:

```shell
synly --headless --host --fs off --audio send --pin 123456 --accept
synly --headless --join --peer workstation --fs off --audio receive --pin 123456
```

输入发送和接收:

```shell
synly --headless --host --fs off --input send --input-edge right --input-hotkey ctrl+alt+shift+esc --pin 123456 --accept
synly --headless --join --peer workstation --fs off --input receive --pin 123456
```

## 配置文件

Synly 使用固定的三文件配置目录:

```text
~/.config/synly/
├── config.toml
├── identity.toml
└── trusted-devices.toml
```

- `config.toml` 保存用户设置和运行参数. 输入方向, 屏幕边缘, 热键, 按键映射和滚动反向都位于 `[input]`.
- `identity.toml` 保存 `device_id`, `private_key`, `public_key`. 文件缺失时自动生成, 已存在但密钥无效时启动失败.
- `trusted-devices.toml` 使用 `[[devices]]` 保存可信设备和会话统计.
- 配置采用严格 schema. 未知字段, 缺失必填字段和旧单文件格式都会导致启动失败, 不会自动迁移或覆盖.

默认跨平台映射为 `Option <-> Win`, `Command <-> Alt`. 映射由发送端应用, 仅在 macOS 与 Windows 之间生效. 两张方向表均支持修改或清空, 但每个目标键只能出现一次.

普通键使用 `a` 到 `z`, `0` 到 `9`, `f1` 到 `f12`, `enter`, `escape`, `backspace`, `tab`, `space`, `minus`, `equal`, `left_bracket`, `right_bracket`, `backslash`, `semicolon`, `apostrophe`, `comma`, `period`, `slash`, `caps_lock`, `insert`, `home`, `page_up`, `delete`, `end`, `page_down` 和方向键名称. 修饰键按平台使用 `left_ctrl`, `left_shift`, `left_option`, `left_command`, `left_alt`, `left_win` 及对应的 `right_` 名称. 实际可用键位仍受源端捕获和目标端注入能力限制.

`reverse_mouse_wheel` 和 `reverse_trackpad` 会同时反转水平与垂直滚动. macOS 能区分普通滚轮和连续触控板滚动. Windows 发送端会将所有滚动视为鼠标滚轮, 因此 `reverse_trackpad` 在 Windows 上不生效. 这些配置在应用启动时读取, 修改后需要重启.

## 配置生效方式

| 配置 | 生效方式 |
|---|---|
| 通知, 日志级别, GUI 和启动行为 | 本地立即生效 |
| 剪贴板, 音频, 输入方向 | 当前会话内 capability 热协商 |
| 输入边缘和紧急热键 | 推进 capability generation 并重建输入辅助通道 |
| 剪贴板大小和缓存限制 | 下一项剪贴板载荷应用 |
| 扫描间隔 | 当前文件任务立即更新 |
| 删除同步 | 确认后更新策略并请求一次远端快照 |
| 文件模式, 路径, 初始来源, 最大目录深度 | 自动断开并重连 |
| 角色, 对侧, 监听端口, 传输限制 | 重建监听器或会话 |
| capability 广播内容 | 保持当前会话并重新发布发现信息 |
| 设备名, 实例名, mDNS 或 LND 设置 | 重新启动发现服务并受控重连当前会话 |
| 撤销当前对侧信任 | 立即断开 |

## 验证

```shell
cargo test
cargo clippy --all-targets --all-features
```

协议热协商测试覆盖 generation 并发更新, stale epoch, ack 和 capability 开关状态. Windows 原生构建应运行 `cargo clippy --all-targets --all-features`, `cargo test` 和 `cargo build --bins`. 输入代理仍需要在普通应用, 管理员应用, UAC 拒绝, 代理崩溃, 签名发布包和安全桌面切换场景中进行真机验证.

## 协议兼容

当前协议版本为 `18`. 本版本不提供旧协议兼容层. 发现结果会携带协议版本, GUI 在连接前禁用不兼容设备.
