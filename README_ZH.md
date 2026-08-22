# Sendmer [![][img_crates]][crates] [![][img_doc]][doc]

[English](README.md) | 中文

Sendmer 是一个基于 [iroh](https://crates.io/crates/iroh) 和 [iroh-blobs](https://crates.io/crates/iroh-blobs) 的轻量级文件传输工具，同时也提供可复用的 Rust 库 API。

它基于 [n0-computer/sendme v0.31.0](https://github.com/n0-computer/sendme/commit/6e50167a8a4d45736179cce3d8d5fd803c87c24e) 演进而来，目前代码已经整理为更清晰的 library + CLI 结构。

## 特性

- 一条命令发送单个文件或整个目录
- 通过 iroh 自动进行 NAT 穿透和打洞，失败时回退到 relay
- 基于 `iroh-blobs` 的 blake3 校验流式传输
- 对外暴露 `send` 和 `receive` 两个 Rust API
- 带有有序会话和结构化错误的版本化传输事件
- 支持 sender 共享上传限速、CLI 进度显示和剪贴板辅助

sendmer 使用 256 位节点 ID，因此 ticket 在 IP 地址变化后仍可继续使用。连接使用 TLS 加密。

## 安装

### Windows（PowerShell）

```powershell
# 安装最新版本
iwr https://raw.githubusercontent.com/bruceblink/sendmer/main/install.ps1 -useb | iex

# 或安装指定版本
$env:SENDMER_VERSION="v0.9.0"
iwr https://raw.githubusercontent.com/bruceblink/sendmer/main/install.ps1 -useb | iex
```

Git Bash、MSYS2 或 Cygwin 请改用 shell 安装器，并确保已安装 `unzip`：

```bash
curl -fsSL https://raw.githubusercontent.com/bruceblink/sendmer/main/install.sh | bash
```

默认安装路径：

```powershell
C:\Users\<用户名>\.sendmer\bin\sendmer.exe
```

安装后：

- 如有需要，将 `$InstallDir` 加入 `PATH`
- 重启终端
- 运行 `sendmer --help`

### Linux / macOS

```bash
# 安装最新版本
curl -fsSL https://raw.githubusercontent.com/bruceblink/sendmer/main/install.sh | bash

# 或安装指定版本
SENDMER_VERSION=v0.9.0 \
curl -fsSL https://raw.githubusercontent.com/bruceblink/sendmer/main/install.sh | bash
```

默认安装路径：

```bash
~/.sendmer/bin/sendmer
```

如有需要，将其加入 shell 配置：

```bash
export PATH="$HOME/.sendmer/bin:$PATH"
```

然后验证：

```bash
sendmer --help
```

### Cargo

```bash
cargo install sendmer --locked
```

## 用法

### 发送

```bash
sendmer send <文件或目录>
```

该命令会启动一个临时 iroh 提供端，导入指定文件或目录，并输出带 ticket 的接收命令。

示例：

```bash
sendmer send ./my-folder
```

典型输出：

```text
imported directory my-folder, 12.3 MiB, hash <hash>
to get this data, use
sendmer receive blob:...
```

发送端会持续运行，直到你使用 `Ctrl+C` 主动停止。停止后会关闭临时 provider，并删除位于系统临时目录下的 blob 存储目录。

### 接收

```bash
sendmer receive <ticket>
```

该命令默认会把数据下载到当前工作目录。
你也可以用 `--output-dir <path>` 指定下载目标目录。

示例：

```bash
sendmer receive <ticket>
```

接收过程默认使用临时 iroh store。只有所有导出条目都报告完成后，才会把文件或目录发布到最终目标；接收失败会清理临时数据，不会替换已有目标。使用 `--cache-dir <path>` 可显式保留失败或取消前已经验证的数据；后续进程接收相同内容时会重新打开该 store，成功导出后则删除已完成的缓存条目。

目录必须包含常规文件。由于当前 collection 格式不能安全保留目录元数据，sendmer 会拒绝空目录、含空子目录的目录和符号链接。

## 常用参数

`send` 和 `receive` 共同支持：

- `--no-progress`：关闭 CLI 进度显示
- `--json-events`：将版本化传输事件信封作为实时刷新的 JSON Lines 输出到 stdout，不显示进度条；日志和人类可读命令结果仍写入 stderr
- `-v` / `-vv`：提高日志详细程度
- `--relay <default|disabled|url>`：控制 relay 使用方式
- `--magic-ipv4-addr <addr>`：绑定固定 IPv4 地址
- `--magic-ipv6-addr <addr>`：绑定固定 IPv6 地址
- `--show-secret`：打印当前进程使用的 secret key
- `--format <hex|cid>`：选择 hash 输出格式；只影响 `send` 打印的 hash，`receive` 会忽略该参数

仅 `receive` 支持：

- `--output-dir <path>`：指定接收文件的输出目录（默认：当前工作目录）
- `--retry-limit <count>`：blob 下载阶段的最大尝试次数（默认：`3`）
- `--retry-backoff-ms <milliseconds>`：两次 blob 下载尝试之间的等待时间（默认：`250`）
- `--connect-timeout-ms <milliseconds>`：每次连接发送端的可选超时
- `--metadata-timeout-ms <milliseconds>`：获取 collection 元数据的可选超时
- `--download-idle-timeout-ms <milliseconds>`：下载流长期没有更新时的可选超时
- `--cache-dir <path>`：显式启用按内容寻址的持久接收缓存，用于跨进程续传
- `--cache-ttl-seconds <seconds>`：记录供后续清理使用的缓存 TTL（默认：`604800`；仅与 `--cache-dir` 一起生效）

### 接收安全语义

本版本唯一支持的冲突策略是 **fail**。最终目标已存在为文件、目录或符号链接时，接收会失败并保持原内容不变；sendmer 不会合并、重命名、跳过或覆盖已有目标。含多个顶层根的外部 collection 也会被拒绝，因为它们不能作为一个原子目标提交。

重试会复用同一次 receive 进程已经获得的数据。跨进程续传必须显式启用：后续 receive 为相同内容配置同一个私有 `--cache-dir`。失败或取消会保留已验证范围，成功接收会删除已完成的缓存条目，且发送端仍需在线。

打开持久缓存时会自动清理过期条目，也可以显式执行同一套维护逻辑：

```bash
sendmer cache prune --cache-dir <path>
```

清理使用每个条目记录的 TTL，跳过活跃条目，并保留损坏或未来 schema 数据而不进行猜测。缓存格式、隐私边界以及中断/重启 E2E 统一定义在[主线设计与开发计划](docs/MAINLINE.md)中。

仅 `send` 支持：

- `--ticket-type <id|RelayAndAddresses|relay|addresses>`：控制 ticket 中包含的地址信息；组合模式当前必须使用精确的 `RelayAndAddresses` 写法
- `--max-upload-rate <bytes-per-second>`：可选地限制发送端所有接收方共享的 payload 总上传速率；不包含协议开销
- `--max-receivers <count>`：可选地限制同时活动的接收方连接数；接收方断开后会释放名额
- `--max-files <count>`：可选地限制共享路径中的普通文件数量；超过上限会在网络和临时存储初始化前拒绝
- `--max-total-size <bytes>`：可选地限制共享路径中普通文件的总 payload 大小；超过上限会在网络和临时存储初始化前拒绝
- `--max-import-memory <bytes>`：可选地限制并行 sender 导入任务估算的普通文件字节预算；这是导入工作集上限，不是进程 RSS 上限
- `--clipboard`：把生成的 `sendmer receive ...` 命令复制到剪贴板（默认启用 `clipboard` feature 时可用）

## 作为库使用

该 crate 同时导出了一组简洁的 Rust API。新的 GUI 或服务集成建议使用 `SendHandle`，
显式关闭临时 provider，而不依赖 iroh 内部句柄：

```rust
use std::path::PathBuf;
use sendmer::{send_handle, SendOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let handle = send_handle(PathBuf::from("./my-file"), SendOptions::default(), None).await?;
    println!("sendmer receive {}", handle.ticket());
    handle.close().await?;
    Ok(())
}
```

旧的 `send` 函数和 `SendResult::shutdown` 仍保留用于兼容。接收端可以使用 `receive`，
或使用带取消能力的 `receive_with_cancellation` 与 `ReceiveOptions`。

调用 `SendHandle::cancel` 可主动撤销共享。它会先停止 provider，再关闭 router，因此撤销后旧
ticket 不能再建立新的接收连接。ticket 只在发送方 router 存活期间作为 bearer capability 有效，
不代表账号、持久 ACL 或撤销列表。

`TransferEvent` 现在是版本化 `TransferEventEnvelope` 的公开别名。每个 send 或 receive 会话
具有一个随机 `session_id`、严格递增的 `sequence`、显式阶段，并且 completed、failed、
cancelled 三种终态最多出现一个。失败事件包含 `TransferErrorCode`、失败阶段、可重试属性和
安全消息。消费者应匹配 `event.event`，不得解析错误文本。参见
[v0.8.0 迁移指南](docs/V0_8_MIGRATION.md)和可编译的
[`event_consumer` 示例](examples/event_consumer.rs)。

库层会 re-export：

- 参数和选项类型
- 传输事件类型与 `EventEmitter`
- `send`、`send_handle`、`receive` 和 `receive_with_cancellation`
- 推荐使用的 `SendHandle`，以及兼容用的 `SendResult` 与 `ReceiveResult`

## 开发

- [DEVELOPMENT.md](DEVELOPMENT.md)
- [主线设计与开发计划](docs/MAINLINE.md)
- [v0.8.0 迁移指南](docs/V0_8_MIGRATION.md)

## License

[GNU Affero 通用公共许可证 v3.0（仅限此版本，AGPL-3.0-only）](LICENSE)

[![Sponsor](https://img.shields.io/badge/sponsor-30363D?style=for-the-badge&logo=GitHub-Sponsors&logoColor=#EA4AAA)](https://github.com/sponsors/bruceblink) [![Buy Me Coffee](https://img.shields.io/badge/Buy%20Me%20Coffee-FF5A5F?style=for-the-badge&logo=coffee&logoColor=FFFFFF)](https://buymeacoffee.com/bruceblink)

## Contributors

<a href="https://github.com/bruceblink/sendmer/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=bruceblink/sendmer" alt="bruceblink/sendmer"/>
</a>

[img_crates]: https://img.shields.io/crates/v/sendmer.svg
[img_doc]: https://img.shields.io/badge/rust-documentation-blue.svg

[crates]: https://crates.io/crates/sendmer
[doc]: https://docs.rs/sendmer/
