# Sendmer [![][img_crates]][crates] [![][img_doc]][doc]

[English](README.md) | 中文

Sendmer 是一个基于 [iroh](https://crates.io/crates/iroh) 和 [iroh-blobs](https://crates.io/crates/iroh-blobs) 的轻量级文件传输工具，同时也提供可复用的 Rust 库 API。

它基于 [n0-computer/sendme v0.31.0](https://github.com/n0-computer/sendme/commit/6e50167a8a4d45736179cce3d8d5fd803c87c24e) 演进而来，目前代码已经整理为更清晰的 library + CLI 结构。

## 特性

- 一条命令发送单个文件或整个目录
- 通过 iroh 自动进行 NAT 穿透和打洞，失败时回退到 relay
- 基于 `iroh-blobs` 的 blake3 校验流式传输
- 对外暴露 `send` 和 `receive` 两个 Rust API
- 支持 CLI 进度显示和剪贴板辅助

sendmer 使用 256 位节点 ID，因此 ticket 在 IP 地址变化后仍可继续使用。连接使用 TLS 加密。

## 安装

### Windows（PowerShell）

```powershell
# 安装最新版本
iwr https://raw.githubusercontent.com/bruceblink/sendmer/main/install.ps1 -useb | iex

# 或安装指定版本
$env:SENDMER_VERSION="v0.4.0"
iwr https://raw.githubusercontent.com/bruceblink/sendmer/main/install.ps1 -useb | iex
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
SENDMER_VERSION=v0.4.0 \
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

接收过程中会先将数据写入系统临时目录下的临时缓存目录，完成后再清理该目录。

## 常用参数

`send` 和 `receive` 共同支持：

- `--no-progress`：关闭 CLI 进度显示
- `-v` / `-vv`：提高日志详细程度
- `--relay <default|disabled|url>`：控制 relay 使用方式
- `--magic-ipv4-addr <addr>`：绑定固定 IPv4 地址
- `--magic-ipv6-addr <addr>`：绑定固定 IPv6 地址
- `--show-secret`：打印当前进程使用的 secret key

仅 `receive` 支持：

- `--output-dir <path>`：指定接收文件的输出目录（默认：当前工作目录）

仅 `send` 支持：

- `--ticket-type <id|relay-and-addresses|relay|addresses>`：控制 ticket 中包含的地址信息
- `--format <hex|cid>`：控制导入后 hash 的输出格式
- `--clipboard`：把生成的 `sendmer receive ...` 命令复制到剪贴板

## 作为库使用

该 crate 同时导出了一组简洁的 Rust API：

```rust
use sendmer::{receive, send, ReceiveOptions, SendOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // let send_result = send(path, SendOptions::default(), None).await?;
    // let receive_result = receive(ticket, ReceiveOptions::default(), None).await?;
    Ok(())
}
```

库层会 re-export：

- 参数和选项类型
- 传输事件类型与 `EventEmitter`
- `send` 和 `receive`
- `SendResult` 与 `ReceiveResult`

## 开发

[DEVELOPMENT.md](DEVELOPMENT.md)

## License

[MIT](LICENSE)

[![Sponsor](https://img.shields.io/badge/sponsor-30363D?style=for-the-badge&logo=GitHub-Sponsors&logoColor=#EA4AAA)](https://github.com/sponsors/bruceblink) [![Buy Me Coffee](https://img.shields.io/badge/Buy%20Me%20Coffee-FF5A5F?style=for-the-badge&logo=coffee&logoColor=FFFFFF)](https://buymeacoffee.com/bruceblink)

## Contributors

<a href="https://github.com/bruceblink/sendmer/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=bruceblink/sendmer" alt="bruceblink/sendmer"/>
</a>

[img_crates]: https://img.shields.io/crates/v/sendmer.svg
[img_doc]: https://img.shields.io/badge/rust-documentation-blue.svg

[crates]: https://crates.io/crates/sendmer
[doc]: https://docs.rs/sendmer/
