# Sendmer

`Sendmer` 基于 [n0-computer/sendme v0.31.0](https://github.com/n0-computer/sendme/commit/6e50167a8a4d45736179cce3d8d5fd803c87c24e)

Crate 地址: <https://crates.io/crates/sendmer>

文档: <https://docs.rs/sendmer>

用于演示如何使用
[iroh](https://crates.io/crates/iroh) 与 [iroh-blobs](https://crates.io/crates/iroh-blobs)
协议在互联网上传输文件和目录。

该仓库同时扮演两种角色：一方面提供面向终端用户的命令行工具（CLI），
另一方面暴露库 API，允许其他 Rust 项目复用基于 iroh 的传输功能。

同时它也可以作为一个独立的工具用于快速拷贝任务。

Iroh 会在可能的情况下负责 hole punching（打洞）和 NAT 穿透，若打洞失败则回退到中继。

Iroh-blobs 提供基于 [blake3](https://crates.io/crates/blake3) 的流校验，并支持断点续传。

`sendmer` 使用 256 位节点 ID，具备位置透明性（节点 IP 变化时票据仍然有效）。连接采用 TLS 加密。

---

Crate 地址: https://crates.io/crates/sendmer
文档: https://docs.rs/sendmer


## 安装

```
cargo install sendmer
```

## 用法

### 发送端

```
sendmer send <文件或目录>
```

该命令会创建一个临时的 [iroh] 节点，提供指定文件或目录的服务，并输出一个可供接收方使用的 ticket。
提供者会一直运行直到被 `Control-C` 终止；终止后会删除临时目录。

当前实现会在当前目录创建临时目录；未来可能改为系统临时目录或其他位置。

### 接收端

```
sendmer receive <ticket>
```

该命令会下载数据并在**当前目录**创建与源相同名称的文件或目录。

下载流程会先在当前目录创建一个以 `.sendmer-` 开头的临时目录，完成下载并校验后再将文件/目录移动到目标位置；完成后删除临时目录。

所有临时目录均以 `.sendmer-` 开头。

---

开发者指南请见： [DEVELOPMENT.md](DEVELOPMENT.md)

## 示例

### 基本发送

```bash
# 发布目录
sendmer send ./my-folder
```

命令会输出一个 ticket，接收方用该 ticket 执行下载。提供者需保持运行直到接收完成（Ctrl-C 结束）。

### 基本接收

```bash
# 使用 ticket 下载
sendmer receive <ticket>
```

默认将数据下载到当前目录，先写入以 `.sendmer-` 开头的临时目录，完成后再移动到最终位置。

### 关闭进度输出

```bash
sendmer send ./file --no-progress
sendmer receive <ticket> --no-progress
```

### 作为库在 Rust 程序中使用

可以通过调用导出的库函数 `start_share` 和 `download` 在其他 Rust 程序中嵌入 `sendmer` 的功能：

```rust
use sendmer::{start_share, download, SendOptions, ReceiveOptions};
#[tokio::main]
async fn main() -> anyhow::Result<()> {
	// start_share(path, SendOptions { ... }, Some(event_emitter)).await?;
	// download(ticket, ReceiveOptions { ... }, Some(event_emitter)).await?;
	Ok(())
}
```

