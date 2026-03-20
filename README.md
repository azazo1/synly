# Synly

Synly 是一个面向局域网的跨平台 Rust CLI，用来发现附近设备、通过 PIN 配对、建立安全 TCP 连接，并持续同步指定文件、文件夹和可选的纯文本剪贴板。

当前版本已经实现：

- 局域网内通过 mDNS 自动发现设备
- 连接发起方 / 接收方 / 双向同步 / 自动协商四种模式
- 客户端先发送请求信息，服务端再为该请求显示一次性 6 位 PIN，客户端随后输入
- 服务端在放行前展示客户端身份、请求模式和同步摘要，并交互确认
- 建立加密连接后用 `notify` 监听目录变化，并保留周期性重扫兜底
- 可选地同步纯文本剪贴板，方向跟随当前发送 / 接收 / 双向 / 自动协商模式
- 支持用 `.synlyignore` 排除不想参与同步的路径，语法兼容 gitignore
- 默认交互式使用，也支持命令行显式指定模式和路径

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
- 长期信任的设备证书管理
- 后台守护进程 / 系统服务集成
- 图形界面

## 安装

需要本机安装 Rust 工具链。

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

### 方式一：完全交互式

在任意一台机器上运行：

```bash
synly
```

程序会依次询问你：

1. 当前设备是发送方、接收方、双向同步还是自动协商
2. 本次是等待别人连接，还是主动连接别人
3. 要同步哪个目录
4. 是否同步纯文本剪贴板

如果没有指定路径，程序会提示你直接回车使用当前文件夹，或者直接输入目标路径；选定后还会再次打印本次同步目录，方便确认。

### 方式二：显式指定模式

把当前目录作为发送源，并等待别人连接：

```bash
synly send . --host
```

把远端内容接收到 `./backup`：

```bash
synly receive ./backup --join
```

双向同步当前目录，并等待别人连接：

```bash
synly both . --host
```

把兜底重扫间隔改成 5 秒：

```bash
synly both . --host --interval-secs 5
```

监听时使用自动协商模式：

```bash
synly auto . --host
```

同时开启文件和纯文本剪贴板双向同步：

```bash
synly both . --host --sync-clipboard
```

## 使用流程

### 服务端（被连接方）

运行：

```bash
synly auto . --host
```

随后 Synly 会：

1. 在局域网中通过 mDNS 广播当前设备
2. 打印当前设备模式、端口，并开始等待请求
3. 收到请求后先展示客户端信息和同步摘要
4. 为这一次请求生成并显示单独的 6 位 PIN
5. 客户端输入 PIN 后，再询问你是否接受本次同步

### 客户端（连接方）

运行：

```bash
synly both . --join
```

随后 Synly 会：

1. 在局域网中搜索可连接的 Synly 设备
2. 列出发现到的设备供你选择
3. 先把本机身份、模式和同步摘要发送给服务端
4. 收到提示后，再输入服务端当前为这次请求显示的 6 位 PIN
5. 等待服务端确认后建立同步

## 命令概览

```text
synly [OPTIONS] [COMMAND]

Commands:
  auto
  send
  receive
  both

Options:
  --host
  --join
  --sync-delete
  --no-sync-delete
  --sync-clipboard
  --no-sync-clipboard
  --interval-secs <SECONDS>
```

### `auto`

本机使用同一个目录同时支持发送和接收，特别适合 `--host` 监听场景。

```bash
synly auto . --host
synly auto ./workspace --join
```

说明：

- `auto` 会把同一个目录同时作为发送目录和接收目录
- 监听连接时，它会根据客户端请求方向自动协商
- 如果双方都支持双向同步，也会协商成双向

### `send`

本机作为发送方，只把本地内容同步给对端。

```bash
synly send ./docs --host
synly send ./a ./b ./c --join
```

说明：

- 如果只传入一个目录，会同步这个目录的内容
- 如果传入多个路径，会把这些路径作为“选定条目”同步
- 多个路径不能有重名的顶层文件名

### `receive`

本机作为接收方，只接收对端内容。

```bash
synly receive ./incoming --host
synly receive ./incoming --join
```

### `both`

本机既能发送，也能接收。

```bash
synly both . --host
synly both . --join
```

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

Synly 可以可选地同步纯文本剪贴板，默认关闭。可以在交互式提示中开启，也可以显式指定：

```bash
synly both . --host --sync-clipboard
synly receive ./incoming --join --sync-clipboard
synly auto . --host --no-sync-clipboard
```

说明：

- 当前只同步纯文本，不同步图片、HTML、RTF 或文件列表
- 剪贴板同步只有在双方都开启时才会生效；如果只有一边开启，连接建立后会明确提示本次不会同步剪贴板
- 剪贴板的发送 / 接收方向跟随当前同步模式和最终协商结果
- 连接建立后会先尝试同步一次当前纯文本剪贴板，之后继续监听新的纯文本变化
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
- `synly send ./docs ./notes ./todo.txt` 这类多路径发送时，目录条目会继续读取各自目录树内的 `.synlyignore`；显式传入的单个文件仍按显式选择处理

### 删除行为

如果当前设备会接收文件，Synly 会额外确认是否同步对端删除，默认是不删除。
也可以显式指定：

```bash
synly receive ./incoming --join --sync-delete
synly both . --host --sync-delete
synly both . --host --no-sync-delete
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
synly send ./docs ./notes ./todo.txt
```

远端会收到三个顶层条目：

```text
docs/
notes/
todo.txt
```

这时删除同步只会影响这些被共享的顶层条目，不会扩散到接收目录里其他不相关内容。

### 示例 4：同时同步目录和纯文本剪贴板

两边都执行并开启剪贴板同步：

```bash
synly both . --sync-clipboard
```

如果只有一边开启 `--sync-clipboard`，文件仍会正常同步，但剪贴板不会生效。

## 安全模型

Synly 当前的安全连接模型是：

1. 先建立 TLS 连接
2. 客户端先发送本机身份、请求模式和同步摘要
3. 服务端为该请求生成一次性 6 位 PIN，并在本地屏幕显示
4. 客户端输入 PIN
5. 双方使用 TLS exporter + PBKDF2 + HMAC，把 PIN 绑定到这一次请求
6. 服务端确认请求后，才开始同步

这意味着：

- PIN 不会以明文在网络中传输
- 仅知道局域网地址还不够，必须拿到 PIN 才能完成配对
- 服务端可以看到请求方身份、模式和同步摘要后再决定是否放行

但你也需要知道当前版本的边界：

- 还没有“长期信任设备”机制
- 还没有持久化证书指纹或设备公钥绑定
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

首次运行时，Synly 会为当前设备生成一个本地设备配置，保存设备 ID 和设备名称。

典型位置：

- macOS: `~/Library/Application Support/synly/device.json`
- Linux: `~/.config/synly/device.json`
- Windows: `%APPDATA%/synly/device.json`

设备名称来源优先级大致为：

1. `SYNLY_DEVICE_NAME`
2. `HOSTNAME` / `COMPUTERNAME`
3. 当前用户名 + 随机后缀

## 示例

### 示例 1：把一台机器的当前目录同步到另一台机器

接收端：

```bash
synly receive . --host
```

发送端：

```bash
synly send . --join
```

### 示例 2：两台机器共享同名工作目录

两边都进入各自项目目录后执行：

```bash
synly auto .
```

一边选择“等待别人连接”，另一边选择“连接局域网中的设备”。

### 示例 3：同步几个离散路径

```bash
synly send ./docs ./scripts ./README.md --host
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
- 剪贴板目前只支持纯文本；非文本内容变化会被忽略
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
