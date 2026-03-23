# Synly

Synly 是一个面向局域网的跨平台 Rust CLI，用来发现附近设备、通过最小明文 bootstrap + PIN + PAKE 建立临时 mTLS，或在已信任设备之间直接使用长期 mTLS，并持续同步指定文件、文件夹和可选的多格式剪贴板。

| Host Mode | Join Mode |
|-----------|-----------|
| [![Host Mode Demo](assets/host.gif)](assets/host.gif) | [![Join Mode Demo](assets/join.gif)](assets/join.gif) |

当前版本已经实现：

- 局域网内通过 mDNS 自动发现设备
- 文件同步支持关闭 / 发送 / 接收 / 双向 / 自动协商五种模式
- 未信任设备先只交换一次性 bootstrap 公钥，服务端显示客户端 bootstrap 指纹 ASCII 图和该会话专属 PIN
- 客户端输入 PIN 后，双方先完成基于 PIN 的 SPAKE2 PAKE，再派生临时 mTLS，在加密信道内传输设备身份、请求模式和同步摘要
- 服务端在看见加密后的客户端摘要后再交互确认是否放行
- host 会对未信任设备的失败配对做超时、指数退避和冷却限制，降低暴力猜 PIN 风险
- 支持在成功输入一次 PIN 后为设备对建立可信设备公钥和根证书绑定，后续连接会优先走 mTLS 并免 PIN
- 建立加密连接后用 `notify` 监听目录变化，并保留周期性重扫兜底
- 支持为当前运行中的 Synly 进程单独指定一个临时名称，便于同机多进程时在发现和配对阶段区分与匹配
- 可选地同步剪贴板中的文本、RTF、HTML、图片，以及限制大小内的文件；剪贴板方向由独立的 `--clipboard off|send|receive|both` 控制，默认关闭，默认允许最多 100 MB 的剪贴板二进制内容
- 文件和大剪贴板内容使用分段流式传输，线上 payload 使用二进制编码而不是 JSON
- 连接意外断开后会自动重连；如果只是某个内容超过大小限制，会跳过那次发送，不会直接退出整个程序
- 支持用 `.synlyignore` 排除不想参与同步的路径，语法兼容 gitignore
- 默认通过启动 TUI 补全参数，也支持通过命令行显式指定模式、目标设备、PIN、自动接受和可信设备策略

## 设计目标

- 尽量少配置，第一次运行就能开始同步
- 尽量使用成熟库，而不是重复造轮子
- 命令行提示直白，适合直接在终端中操作
- 支持 macOS / Linux / Windows 这类常见桌面环境

## 当前状态

这是一个可编译、可运行的原型版本，核心配对和同步流程已经打通，但还不是生产级同步工具。

它更适合：

- 两台在同一局域网里的机器临时建立同步关系
- 在终端中手动确认一次同步连接
- 同步中小规模目录，或显式指定的一组文件/目录

如果你需要下面这些能力，目前还没有：

- 断点续传
- 历史版本 / 冲突合并策略
- 可信设备公钥轮换 / 撤销 / 证书链管理
- 后台守护进程 / 系统服务集成
- 图形界面

## 编译环境准备

Synly 本体可以在 macOS / Linux / Windows 上编译；“音频同步”依赖 Opus，并且当前的原生音频后端支持情况是：

- Windows：已实现发送和接收
- macOS：已实现发送和接收，采集走系统音频 tap，当前要求 macOS 14.0+，且暂不支持手动选择采集设备
- Linux：音频后端暂未实现；如果只使用文件 / 剪贴板同步，不受影响

也就是说：

- 不开音频同步时，三大桌面平台都可以正常编译和使用文件 / 剪贴板同步
- 开启音频同步相关编译时，三大平台都需要准备 Opus 开发库
- 真正运行音频发送 / 接收时，目前建议按 Windows 或 macOS 环境准备

### Windows

推荐使用 MSVC 工具链。

1. 安装 Rust：

```powershell
rustup default stable-x86_64-pc-windows-msvc
```

2. 安装 Visual Studio 2022 Build Tools，至少勾选：

- `Desktop development with C++`
- Windows 10/11 SDK

3. 安装 `vcpkg`，并安装 Opus：

```powershell
vcpkg install opus:x64-windows-static
```

4. 设置 `VCPKG_ROOT` 指向你的 `vcpkg` 根目录；如果以前手动设置过 `OPUS_LIB_DIR` / `OPUS_DIR`，建议确认它们没有指向已经失效的旧路径。

```powershell
$env:VCPKG_ROOT="C:\path\to\vcpkg"
```

如果你使用的是持久化环境变量，也可以把 `VCPKG_ROOT` 写入系统环境变量。

### macOS

需要 Rust、Xcode Command Line Tools、`pkg-config` 和 `opus`：

```bash
xcode-select --install
brew install pkg-config opus
rustup default stable
```

说明：

- `build.rs` 会优先通过 `pkg-config` 查找 Opus
- `build.rs` 还会用 `clang` 编译 `native/macos_audio.m`，并链接 `Foundation`、`AudioToolbox`、`CoreAudio` 等系统 framework
- 运行音频采集需要 macOS 14.0 或更高版本
- 当前 macOS 采集固定使用系统音频 tap，暂不支持手动选择采集设备

### Linux

需要 Rust、C 编译器、`pkg-config` 和 Opus 开发包。

Debian / Ubuntu：

```bash
sudo apt update
sudo apt install -y build-essential pkg-config libopus-dev
rustup default stable
```

Fedora：

```bash
sudo dnf install -y gcc gcc-c++ make pkgconf-pkg-config opus-devel
rustup default stable
```

说明：

- `build.rs` 会通过 `pkg-config` 解析 Opus 的头文件 / 链接参数
- 当前 Linux 音频后端还没有实现；如果只使用文件 / 剪贴板同步，不受影响

## 安装

准备好上面的编译环境后，执行：

```bash
cargo build --release
```

生成的可执行文件位于：

```text
target/release/synly
```

也可以直接用开发模式运行：

```bash
cargo run --
```

## 快速开始

### 方式一：启动 TUI

在任意一台机器上运行：

```bash
synly
```

如果启动参数还不完整，程序会直接进入启动 TUI，你可以在里面选择：

1. 文件同步模式：`--fs <send|receive|both|auto|off>`，默认是 `off`
2. 连接方式：`--host` 或 `--join`
3. 共享目录或接收目录（`off` 模式不需要）
4. 剪贴板同步方向：`off` / `send` / `receive` / `both`
5. 其他可选项，例如当前进程名、删除同步、音频模式等

如果你已经通过命令行把连接方式等必要参数传完整，程序就会直接启动，不再额外弹出启动询问；不传 `--fs` 时默认按 `off` 处理。

### 方式二：显式指定模式

把当前目录作为发送源，并等待别人连接：

```bash
synly --fs send . --host
```

把远端内容接收到 `./backup`：

```bash
synly --fs receive ./backup --join
```

双向同步当前目录，并等待别人连接：

```bash
synly --fs both . --host
```

把兜底重扫间隔改成 5 秒：

```bash
synly --fs both . --host --interval-secs 5
```

只同步共享目录下两层以内的内容：

```bash
synly --fs both . --host --max-folder-depth 2
```

监听时使用自动协商模式：

```bash
synly --fs auto . --host
```

同时开启文件和剪贴板双向同步：

```bash
synly --fs both . --host --clipboard both
```

给当前运行中的 Synly 进程指定一个临时名称，方便对端通过名称匹配：

```bash
synly --name worker-a --fs both . --host
```

只同步剪贴板，不同步文件：

```bash
synly --fs off --host --clipboard both
```

如果你明确不想进入启动交互，可以加上 `--no-interact`。这时如果启动参数不完整，程序会直接报错，并列出当前还缺哪些参数，例如：

```bash
synly --fs receive --no-interact
```

### 方式三：第一次全参数建立可信设备

接收端先启动，并固定本次 PIN、自动接受、允许建立可信设备绑定：

```bash
synly --fs auto . --host --pin 123456 --accept --trust-device
```

连接端指定目标设备，并直接使用同一个 PIN：

```bash
synly --name laptop-sync --fs auto . --join --peer workstation --pin 123456 --trust-device
```

如果这次 PIN 认证成功，并且服务端在确认时选择 `T`、客户端也同意信任服务端，那么双方都会保存对端的身份公钥和 TLS 根证书。

### 方式四：后续免 PIN 自动运行

接收端：

```bash
synly --fs auto . --host --trusted-only
```

连接端：

```bash
synly --fs auto . --join --peer workstation --trusted-only
```

这时如果双方之前已经互相建立过可信设备绑定，就不会再询问 PIN，而是直接走长期 mTLS；服务端也会默认自动接受这次同步。

## 使用流程

### 服务端（被连接方）

运行：

```bash
synly --fs auto . --host
```

随后 Synly 会：

1. 在局域网中通过 mDNS 广播当前设备
2. 打印当前设备模式、端口，并开始等待请求
3. 如果连接方已经被信任，就直接通过长期 mTLS + 身份签名完成认证，并默认自动接受本次同步
4. 如果连接方尚未被信任，先只接收一个最小 bootstrap 请求，并显示客户端 bootstrap 指纹 ASCII 图、本次会话核对图和该请求专属的 6 位 PIN
5. 客户端输入 PIN 后，双方先完成 SPAKE2 PAKE，再切换到临时 mTLS；只有这之后，服务端才会看到对端设备身份、请求模式和同步摘要
6. 对未受信任设备，如果没有传 `--accept`，再询问你是否接受本次同步；输入 `T` 表示接受并信任该客户端，`Y` 表示只接受本次，`n` 表示拒绝

### 客户端（连接方）

运行：

```bash
synly --fs both . --join
```

随后 Synly 会：

1. 在局域网中搜索可连接的 Synly 设备
2. 如果没有传 `--peer`，列出发现到的设备供你选择；传了 `--peer` 时会按进程名、设备名、设备 ID 前缀或 IPv4 地址自动匹配
3. 如果双方已有可信设备绑定，就直接通过长期 mTLS + 身份签名免 PIN 建立认证，服务端会默认自动接受这次同步
4. 否则客户端先生成一次性 bootstrap 指纹 ASCII 图并发起最小请求，服务端随后显示相同的客户端 bootstrap 图、本次会话核对图以及该会话专属的 6 位 PIN
5. 客户端核对图形后输入 PIN，双方完成 SPAKE2 PAKE，验证彼此的 key confirmation，再派生临时 mTLS，并只在这条信道里发送设备身份和同步摘要
6. 如果服务端选择了信任客户端，而本机还没保存该服务端，客户端会再询问是否也信任服务端；传了 `--trust-device` 时会自动同意
7. 完成确认后建立同步

## 命令概览

```text
synly [OPTIONS] [PATH ...]

Options:
  --no-interact
  --fs <MODE>
  --name <NAME>
  --host
  --join
  --sync-delete
  --no-sync-delete
  --clipboard <MODE>
  --audio <MODE>
  --interval-secs <SECONDS>
  --max-folder-depth <DEPTH>
  --peer <QUERY>
  --port <PORT>
  --pin <PIN>
  --accept
  --trust-device
  --trusted-only
  --discovery-secs <SECONDS>
```

其中：

- `PATH` 是文件同步路径；`--fs send` 可以传多个路径，`--fs receive` / `--fs both` / `--fs auto` 只能传一个目录，`--fs off` 不需要路径
- 不传 `--fs` 时默认就是 `off`
- `--name` 是当前 Synly 进程的临时名称，只作用于本次运行，不会写回配置；`--peer` 可以按这个名称匹配
- 剪贴板和音频方向仍然分别通过 `--clipboard <MODE>`、`--audio <MODE>` 独立指定

### `--fs auto`

本机使用同一个目录同时支持发送和接收，特别适合 `--host` 监听场景。

```bash
synly --fs auto . --host
synly --fs auto ./workspace --join
```

说明：

- `--fs auto` 会把同一个目录同时作为发送目录和接收目录
- 监听连接时，它会根据客户端请求方向自动协商
- 如果双方都支持双向同步，也会协商成双向

### `--fs send`

本机作为发送方，只把本地内容同步给对端。

```bash
synly --fs send ./docs --host
synly --fs send ./a ./b ./c --join
```

说明：

- 如果只传入一个目录，会同步这个目录的内容
- 如果传入多个路径，会把这些路径作为“选定条目”同步
- 多个路径不能有重名的顶层文件名

### `--fs receive`

本机作为接收方，只接收对端内容。

```bash
synly --fs receive ./incoming --host
synly --fs receive ./incoming --join
```

### `--fs both`

本机既能发送，也能接收。

```bash
synly --fs both . --host
synly --fs both . --join
```

### `--fs off`

关闭文件同步；适合只运行剪贴板同步、只运行音频，或者单纯先完成配对。

```bash
synly --fs off --host
synly --fs off --join --clipboard both
```

说明：

- `off` 模式不需要传目录路径
- 文件同步会完全关闭
- 剪贴板和音频仍然可以单独开启

## 同步语义

当前同步逻辑是“文件系统事件监听 + 清单比对 + 传输差异文件”，并保留周期性重扫作为兜底。

具体行为如下：

- 默认使用 `notify` 监听本地目录变化
- 默认每 3 秒做一次全量重扫，避免错过底层文件系统事件
- 文件内容通过 SHA-256 比较
- 文件修改时间和可执行位变化也会触发重新同步
- 目录会在传输前自动创建
- 临时文件使用 `.synly.part` 后缀，完成后原子替换目标文件
- 接收目录下的 `.synly/` 会被保留给 Synly 自己使用，并且永远不会参与同步
- `.DS_Store` 和 `desktop.ini` 会被自动忽略，不参与同步
- 符号链接会被忽略
- `.git` 目录会被忽略
- 任意层级的 `.synlyignore` 都会生效，使用 gitignore 兼容语法；被忽略的路径不会参与发送、接收或删除同步

### 剪贴板同步

Synly 可以可选地同步剪贴板，默认关闭。通过 `--clipboard <off|send|receive|both>` 独立指定方向：

```bash
synly --fs both . --host --clipboard both
synly --fs receive ./incoming --join --clipboard receive
synly --fs send ./docs --host --clipboard send
synly --fs off --host --clipboard both
```

说明：

- 当前会尽量同步纯文本、RTF、HTML、图片，以及普通文件剪贴板；富文本最终能否完整落到目标应用，还取决于目标操作系统和应用对对应格式的支持
- 剪贴板方向和文件同步方向已经解耦；是否同步文件由 `--fs <send|receive|both|auto|off>` 决定，是否同步剪贴板由 `--clipboard` 单独决定
- 剪贴板同步只有在双方方向能对上时才会生效；例如一端 `send` 需要另一端至少 `receive`
- 不传 `--clipboard` 时默认是 `off`
- 如果你只想同步剪贴板，可以使用 `synly --fs off --clipboard ...`
- 连接建立后会先尝试同步一次当前剪贴板，之后继续监听新的剪贴板变化
- 剪贴板文件只同步普通文件，不同步目录、符号链接或其他非常规条目；被跳过的条目会打印原因
- 剪贴板文件会先落地到本机配置里的缓存目录下，再挂到系统剪贴板，便于跨机器粘贴；默认目录是配置目录下的 `clipboard-cache/`
- 单个剪贴板文件会受配置项 `clipboard.max_file_bytes` 限制；超过上限的文件不会同步，并会输出原因
- 如果设置了 `clipboard.max_cache_bytes`，剪贴板缓存总占用超过上限后，会按最早出现的顺序清理较旧批次，并尽量保留最新一次缓存
- 双向模式下如果两边几乎同时复制了不同内容，最终结果取决于最后到达的一次更新

### `.synlyignore`

在共享目录里放置 `.synlyignore`，即可排除指定路径。它使用和 `.gitignore` 基本一致的语法，例如：

```gitignore
node_modules/
*.log
dist/
!important.log
```

说明：

- 规则会在对应目录及其子目录中生效
- 双向或接收场景下，本地 `.synlyignore` 也会阻止这些路径被拉取或被删除
- `synly --fs send ./docs ./notes ./todo.txt` 这类多路径发送时，目录条目会继续读取各自目录树内的 `.synlyignore`；显式传入的单个文件仍按显式选择处理

### 删除行为

如果当前设备会接收文件，Synly 会额外确认是否同步对端删除，默认是不删除。
也可以显式指定：

```bash
synly --fs receive ./incoming --join --sync-delete
synly --fs both . --host --sync-delete
synly --fs both . --host --no-sync-delete
```

单向同步时：

- 只有在当前接收端明确开启“删除同步”后，如果远端删除了某个共享文件，本地才会处理对应删除
- 这里的“删除”不会直接抹掉文件，而是移动到接收目录下的 `.synly/deleted/`
- `.synly/deleted/` 会按删除批次分桶保存，避免同名文件互相覆盖

双向同步时：

- 默认仍不自动传播删除；只有当前设备开启“删除同步”后，才会应用对端删除
- 如果希望双方的删除都能互相传播，需要两边都开启删除同步
- 这里的“删除”同样不会直接抹掉文件，而是移动到接收目录下的 `.synly/deleted/`

自动协商模式时：

- 它使用单个共享目录同时承担发送和接收
- 实际同步方向由双方握手结果决定

### 多路径发送

当你使用：

```bash
synly --fs send ./docs ./notes ./todo.txt
```

远端会收到三个顶层条目：

```text
docs/
notes/
todo.txt
```

这时删除同步只会影响这些被共享的顶层条目，不会扩散到接收目录里其他不相关内容。

### 示例 4：同时同步目录和多格式剪贴板

两边都执行并开启剪贴板同步：

```bash
synly --fs both . --clipboard both
```

如果只有一边开启，或者双方方向对不上，文件仍会正常同步，但剪贴板不会生效。

### 示例 5：仅同步剪贴板

两边都执行：

```bash
synly --fs off --clipboard both
```

这时不会同步任何文件，只会按照 `--clipboard` 指定的方向同步剪贴板。

## 安全模型

Synly 当前的安全连接模型是：

1. 如果双方之前已经互相保存过对端身份公钥和根证书，就直接建立长期 mTLS
2. 在长期 mTLS 之上，客户端和服务端都会再用 TLS exporter + 长期身份私钥签名，把应用层身份绑定到这一次会话
3. 每台设备本地都会生成一对长期身份密钥，并从它派生稳定的设备根证书
4. 如果还没有可信公钥绑定，客户端先生成一次性 `X25519` bootstrap 密钥，只把 bootstrap 公钥发给服务端，不发送设备身份、请求模式和同步摘要
5. 服务端显示客户端 bootstrap 指纹 ASCII 图、本次会话核对图，以及该 bootstrap 会话专属的 6 位 PIN，或者使用 `--pin` 指定的固定 PIN
6. 客户端核对图形后输入 PIN，双方先用 `SPAKE2` 完成基于 PIN 的 PAKE，并做一次显式 key confirmation
7. 双方再用 `X25519 shared secret + PAKE key + request_id + 双方 bootstrap 公钥` 派生一次性临时 mTLS 根证书和双端叶子证书
8. 只有在临时 mTLS 建好以后，客户端才会发送本机身份、请求模式和同步摘要
9. host 会对未信任设备的失败尝试做超时、退避、失败次数限制和冷却，降低在线猜 PIN 风险
10. 如果这次 PIN 认证成功，并且服务端在确认时选择信任客户端，客户端随后也同意信任服务端，双方就会保存彼此的长期身份公钥和设备根证书；后续会话必须同时通过长期 mTLS 和对应私钥签名，才会被当作可信设备
11. 服务端确认请求后，才开始同步

这意味着：

- PIN 不会以明文在网络中传输
- 未信任设备在 PIN 前不会暴露设备身份、同步模式或工作区摘要
- 仅知道局域网地址还不够，必须同时拿到 PIN，并通过 PAKE 和 bootstrap 图核对，才能安全完成首次配对
- 已建立可信设备后，后续会话仍然会把签名绑定到这一次 mTLS 会话，不能直接重放旧报文
- 攻击者即使试图在后续连接里双端终止 TLS，也无法通过 mTLS 冒充已受信设备并读取同步明文
- 未信任设备路径带有配对超时、退避和冷却窗口，固定 PIN 场景也更难被持续在线爆破
- 服务端可以看到请求方身份、模式和同步摘要后再决定是否放行

但你也需要知道当前版本的边界：

- 首次用 PIN 建立信任时，仍然建议人工核对双方显示的 bootstrap 图和会话图；如果完全不核对，第一次信任依然属于 TOFU
- PAKE 能显著降低主动中间人风险，但它不能替代“确认你连的是你想连的那台设备”这件事；如果用户选错设备或信错屏幕，仍然可能把首次信任授给错误对象
- 当前的失败限制是按来源 IP 做的，在 NAT 或多人共享出口地址的环境里可能会互相影响
- 可信设备材料目前是按“设备对”保存的，不带轮换和吊销机制
- 根证书是设备自签发身份根，不依赖外部 CA 或硬件信任根
- 更适合在可信局域网和人工确认场景下使用

## 设备发现

Synly 使用 mDNS 广播和发现设备，服务类型为：

```text
_synly._tcp.local.
```

当前实现优先使用非回环 IPv4 地址。

如果局域网里搜不到设备，优先检查：

- 两台机器是否在同一个子网
- 本机防火墙是否拦截 mDNS 或 TCP 监听端口
- 网络环境是否禁用了局域网广播

## 配置文件

首次运行时，Synly 会为当前设备生成一个本地配置文件，保存设备信息、剪贴板策略和传输大小限制。

典型位置：

- macOS: `~/Library/Application Support/synly/config.toml`
- Linux: `~/.config/synly/config.toml`
- Windows: `%APPDATA%/synly/config.toml`

如果旧版本目录下已经存在 `device.json`，首次运行新版本时会自动迁移到 `config.toml`。

一个典型配置如下：

```toml
[device]
device_id = "2d0d69d8-0f7f-40e4-8fd3-fd0a29a2ed84"
device_name = "workstation"
identity_private_key = "MC4CAQAwBQYDK2VwBCIEIOt5..."
identity_public_key = "0i0s2v8kP4q2Tf8s0QylhKf5q7H7YBfQGfJY8y1zPM0"

[clipboard]
max_file_bytes = 104857600
max_cache_bytes = 536870912
cache_dir = "clipboard-cache-custom"

[transfer]
max_meta_bytes = 20971520
max_frame_data_bytes = 134217728
max_clipboard_bytes = 104857600

[[trusted_devices]]
device_id = "6fce44a6-2a07-4f72-9192-a4ec4a1e6df0"
device_name = "laptop"
public_key = "wV4Vj7a7VQxgq9b2oS9Q6I72gq8sSdlx6a1aB6V8n3A"
tls_root_certificate = "MIIB...base64-no-pad..."
trusted_at_ms = 1763651605123
last_seen_ms = 1763651888123
successful_sessions = 3
```

其中：

- `clipboard.max_file_bytes` 是单个剪贴板文件的大小上限，单位为字节
- `clipboard.max_cache_bytes` 可选；是剪贴板缓存目录总占用上限，单位为字节；超过后会按最早出现顺序清理旧缓存
- `clipboard.cache_dir` 可选；可以写绝对路径，也可以写相对配置目录的路径
- 未设置 `clipboard.cache_dir` 时，剪贴板文件缓存默认保存在同一配置目录下的 `clipboard-cache/`
- `transfer.max_meta_bytes` 是单帧元数据上限，单位为字节
- `transfer.max_frame_data_bytes` 是单个二进制帧的数据上限，单位为字节
- `transfer.max_clipboard_bytes` 是单次剪贴板二进制总载荷上限，单位为字节
- `device.identity_private_key` / `device.identity_public_key` 是当前设备的长期身份密钥
- `trusted_devices` 可选；只有在一次 PIN 认证成功后，本机明确同意信任对端时，这里才会出现记录；以后会用这里保存的公钥和根证书建立 mTLS，并校验对端签名

设备名称来源优先级大致为：

1. `SYNLY_DEVICE_NAME`
2. `HOSTNAME` / `COMPUTERNAME`
3. 当前用户名 + 随机后缀

如果你在启动时传了 `--name`，那只是覆盖“本次运行中的进程名称”，用于发现列表和 `--peer` 匹配；它不会改写这里的设备名配置。

## 示例

### 示例 1：把一台机器的当前目录同步到另一台机器

接收端：

```bash
synly --fs receive . --host
```

发送端：

```bash
synly --fs send . --join
```

### 示例 2：两台机器共享同名工作目录

两边都进入各自项目目录后执行：

```bash
synly --fs auto .
```

一边选择“等待别人连接”，另一边选择“连接局域网中的设备”。

### 示例 3：同步几个离散路径

```bash
synly --fs send ./docs ./scripts ./README.md --host
```

## 开发

常用命令：

```bash
cargo fmt
env -u RUSTC_WRAPPER cargo check
env -u RUSTC_WRAPPER cargo test --quiet
env -u RUSTC_WRAPPER cargo clippy --all-targets --all-features -- -D warnings
```

## 已知限制

- 只做文件覆盖，不做三方合并
- 双向模式下如果两边同时修改同一个文件，最后结果取决于后到达的一次同步
- 剪贴板虽然支持文本、富文本、图片和普通文件，但不同操作系统与应用对富文本 / HTML / 图片格式的支持仍可能不完全一致
- 目前只支持 `.synlyignore`，还没有全局忽略规则或更细粒度策略
- 大目录初次同步会比较慢，因为需要计算完整清单和哈希
- 目前没有带宽限制或并发传输调优

## 后续方向

比较值得继续补的能力有：

- 更丰富的忽略规则来源和全局配置
- 冲突检测与提示
- 设备长期信任和证书固定
- 更稳定的断线恢复
- 图形界面或 TUI

## 许可证

暂未添加许可证文件；如果你准备公开分发，建议在仓库中补充明确的 License。
