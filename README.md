# Synly

Synly 是一个面向局域网的跨平台同步应用. 默认启动 Slint GUI, 可在系统托盘后台运行, 并在当前连接中动态调整剪贴板, 音频和输入方向. 文件工作区或角色等会影响会话边界的设置会自动安全断开并重连.

Synly 支持 Windows, macOS 和 Linux. 文件与剪贴板同步可在三大平台使用. 音频运行时目前支持 Windows 和 macOS. 输入同步目前支持 Windows 和 macOS.

## 主要功能

- Slint 主窗口和原生系统托盘.
- 状态, 设备, 同步, 安全, 设置, 日志和 About 页面.
- mDNS 与 LND 设备发现聚合.
- Android 客户端通过 mDNS/LND 发现桌面端, 使用同一套 PIN 配对与 mTLS 协议.
- 未信任设备使用 bootstrap 指纹, SPAKE2 PIN 和临时 mTLS 完成配对.
- 可信设备使用身份公钥和长期 mTLS 免 PIN 重连.
- 文件同步支持 off, send, receive, both 和 auto.
- 剪贴板同步支持文本, RTF, HTML, 图片和受大小限制的文件.
- 音频支持单向 send 和 receive.
- 输入支持单向 send 和 receive, 包含边缘切换和紧急收回热键, 以及面向光标捕获游戏的光标模式(相对增量注入, 支持手动开关与自动检测).
- 剪贴板, 音频和输入模式使用 protocol 17 capability generation 热协商.
- 文件扫描间隔和删除策略可在当前会话中更新.
- 角色, 对侧, 工作区和监听端口变化会自动重建会话.
- host 支持多设备同时接入: 剪贴板在多会话间广播并防回音防洪流, 文件/音频/输入由单一活跃会话承载, 活跃会话断开后自动提升已信任设备, UI 支持会话列表, 逐个断开与手动切换活跃会话.
- 单实例运行. 重复启动会激活已有窗口.
- 支持关闭到托盘, 启动隐藏, 恢复上次会话和登录启动.
- 隐藏窗口收到配对请求时显示不含 PIN 的系统通知, 点击后恢复对应 modal.
- 窗口尺寸会持久化并在下次启动时恢复.
- tracing 同时输出到终端, 每日滚动日志文件和 GUI 环形日志缓冲区, 日志级别可实时调整.

## 安全顺序

未信任设备在 PIN 之前交换一次性 bootstrap 公钥和主动方声明的 device_name. 双方核对 bootstrap 指纹和会话 randomart 后, 使用 PIN 完成 SPAKE2. 设备身份, 工作区摘要和能力信息只在临时 mTLS 建立后传输, bootstrap 阶段的名称仅用于提示并会在认证后重新校验.

固定 PIN 只保存在当前进程内存中. PIN, 密钥, PAKE 数据和剪贴板内容不会写入 tracing 日志.

可信设备可保存对侧身份公钥和 TLS 根证书. 撤销当前对侧信任会立即断开连接, 后续可信重连会被拒绝.

## 构建

Rust 版本需要满足 Slint 1.17.1 的要求. 当前开发基线使用 Rust 1.97.1.

```shell
just dist
```

开发运行:

```shell
cargo run --
```

发布产物位于 `dist/`.

### GitHub 发布

发布工作流在 branch 和 pull request 上执行跨平台编译, 在 tag 或手动指定已有 tag 时生成发布产物. Linux 只做 release 编译, Windows 上传带 exe 图标的 zip, macOS 上传 Intel 和 Apple Silicon 的 app dmg.

创建版本时先提交 `docs/changelog/VERSION.md`, 再创建 annotated tag:

```shell
git tag -a "v0.1.0" --cleanup=verbatim -F "docs/changelog/0.1.0.md"
git push origin main --follow-tags
```

也可以在 GitHub Actions 手动填写已有 tag. 留空时只运行构建并上传 Actions artifact, 不创建 Release.

### Windows

推荐使用 MSVC 工具链和 Windows 10/11 SDK. 音频依赖 Opus.

```powershell
rustup default stable-x86_64-pc-windows-msvc
vcpkg install opus:x64-windows-static
$env:VCPKG_ROOT="C:\path\to\vcpkg"
just dist
```

Windows 主 GUI 使用 `asInvoker` manifest 以普通权限运行. 需要管理员输入能力时, 若主进程已持有提升令牌, GUI 会直接启动继承该令牌的隐藏输入代理, 不显示 UAC. 否则 GUI 通过 `ShellExecuteW("runas")` 启动当前 `synly.exe` 的隐藏输入代理子进程, 并使用命名管道完成握手. 主进程与子进程来自同一个构建产物, 避免 IPC 协议版本错配. 配置 `input.elevate_on_start = true` 后, 主实例会在恢复会话前请求 UAC, 授权失败时本次启动直接失败.

GUI 和提权子进程通过随机命名管道与随机 token 通信. 管道 DACL 只允许当前用户和 SYSTEM, 双方校验 IPC 版本, PID, session ID, 映像路径和安装目录. Windows release 不要求 Authenticode 签名, 因此请仅从可信来源获取程序, 并避免在其他用户可写目录中运行. 管道断开, 心跳超时, 子进程退出或进入非 `Default` 输入桌面时会立即释放输入状态.

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

### Android

Android 端是剪贴板同步客户端, 仅支持局域网内主动连接桌面 host. 它复用 `crates/synly-core` 的协议与加密实现, 通过 uniffi 生成 Kotlin 绑定, UI 使用 Jetpack Compose. Android 10+ 限制后台应用读取剪贴板, 因此后台同步依赖前台服务监听剪贴板变化, 并通过透明的 `ClipboardReadActivity` 短暂抢占前台焦点完成读取 (哈希去重), 需要用户授予"显示在其他应用上层"权限; 前台服务持有网络会话并负责重连.

v1 同步范围为文本, HTML 与 PNG 图片, 不支持文件与 RTF. 图片写入剪贴板时通过 FileProvider 提供 content URI. Android 12 及以上每次读取剪贴板时系统可能显示提示, 这是平台行为.

构建 Android 核心库与绑定:

```shell
just android-core
```

构建 debug APK:

```shell
just android-build
```

也可以使用 Android Studio 打开 `android/` 目录直接构建. 首次配对时, 在桌面端确认 PIN 与指纹, 手机端输入同一 PIN 并核对指纹; 配对成功后双方会保存长期 mTLS 信任, 之后免 PIN 重连.

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

## 命令行启动

`host` (别名 `listen`) 和 `join` (别名 `connect`) 子命令可以一行指定会话角色, 不需要修改配置中的角色字段:

```shell
synly host
synly join demo-device
synly connect 192.168.1.20:8080
```

未搭配 `--headless` 时, 子命令启动 GUI 并自动执行对应操作: host 开始监听, join 开始连接. `join` 的对端参数 `peer` 可以省略, 缺省时使用配置中的 `peer_query`. CLI 指定的角色和对端只对本次启动生效, 不会写入配置文件.

`--headless` 可以放在子命令之前或之后. 搭配 `--headless` 时进入静默模式, 不进行任何终端询问, 并只允许已建立长期 mTLS 信任的设备连接 (CLI 会自动把 `trusted_only` 视为 true). 请先在 GUI 中完成 PIN 配对并保存双方信任.

```shell
synly --headless host
synly --headless join demo-device
```

仅使用 `--headless` 而不带子命令时, 全部会话参数仍从 `config.toml` 读取. 配置缺少角色, 工作区, 初始来源或可信策略时, 会在启动会话前直接失败.

## 配置文件

Synly 使用固定的三文件配置目录:

```text
~/.config/synly/
├── config.toml
├── identity.toml
└── trusted-devices.toml
```

- `config.toml` 保存用户设置和运行参数. 输入方向, 屏幕边缘, 热键, 启动提权, 按键映射, 滚动反向, 按住拦截开关, 光标模式(`cursor_mode`, 三选一: `desktop`/`auto`/`game`)都位于 `[input]`.
- `identity.toml` 保存 `device_id`, `private_key`, `public_key`. 文件缺失时自动生成, 已存在但密钥无效时启动失败.
- `trusted-devices.toml` 使用 `[[devices]]` 保存可信设备和会话统计.
- 配置采用严格 schema. 未知字段, 缺失必填字段和旧单文件格式都会导致启动失败, 不会自动迁移或覆盖.

默认跨平台映射为 `Option <-> Win`, `Command <-> Alt`. 映射由发送端应用, 仅在 macOS 与 Windows 之间生效. 两张方向表均支持修改或清空; 多个来源键可以映射到同一个目标键, 同时按下时后按的来源键会被忽略, 直到最后一个来源键松开才释放目标键.

普通键使用 `a` 到 `z`, `0` 到 `9`, `f1` 到 `f12`, `enter`, `escape`, `backspace`, `tab`, `space`, `minus`, `equal`, `left_bracket`, `right_bracket`, `backslash`, `semicolon`, `apostrophe`, `comma`, `period`, `slash`, `caps_lock`, `insert`, `home`, `page_up`, `delete`, `end`, `page_down` 和方向键名称. 修饰键按平台使用 `left_ctrl`, `left_shift`, `left_option`, `left_command`, `left_alt`, `left_win` 及对应的 `right_` 名称. 实际可用键位仍受源端捕获和目标端注入能力限制.

`reverse_mouse_wheel` 和 `reverse_trackpad` 会同时反转水平与垂直滚动. macOS 能区分普通滚轮和连续触控板滚动. Windows 发送端会将所有滚动视为鼠标滚轮, 因此 `reverse_trackpad` 在 Windows 上不生效. 这些配置在应用启动时读取, 修改后需要重启.

`elevate_on_start` 仅在 Windows 生效. 设置为 `true` 后, GUI 主实例和 Headless 都会在启动会话前请求 UAC 并启动管理员输入代理. macOS 和 Linux 会保存但忽略该字段.

接收端启用游戏光标模式后, 远端 `Motion` 不再换算成绝对坐标, 而是直接注入相对增量, 使锁定光标并读取相对移动的游戏(例如 MC 的 3D 光标, FPS 视角)能够跟随控制端鼠标. 设置中可选择桌面光标, 自动切换或游戏光标: 桌面光标固定绝对注入, 游戏光标固定相对注入, 自动切换按前台光标捕获状态动态选择并自动释放. 全屏本身不算捕获状态, 自动切换只在系统光标被隐藏或范围被裁剪时生效, 游戏暂停菜单或设置界面(普通鼠标状态)不会触发. 该模式为单机或无反作弊游戏设计, 带 EAC/BattlEye/Vanguard 等反作弊的联机游戏可能忽略注入输入或存在封号风险. 固定游戏模式下边缘返回失效, 使用紧急收回热键返回; 自动切换模式以及桌面模式被锁后的临时回退, 会在前台捕获状态结束时自动释放控制.

`elevate_on_start` 是严格配置 schema 的必填字段. 现有 `config.toml` 需要显式补充该字段, 本版本不执行自动迁移.

## 配置生效方式

| 配置 | 生效方式 |
|---|---|
| 通知, 日志级别, GUI 和启动行为 | 本地立即生效 |
| 剪贴板, 音频, 输入方向 | 当前会话内 capability 热协商 |
| 输入边缘和紧急热键 | 推进 capability generation 并重建输入辅助通道 |
| 光标模式(桌面/自动/游戏) | 推进 capability generation 并重建输入辅助通道 |
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
