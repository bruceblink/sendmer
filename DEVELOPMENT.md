# 开发者指南 / Developer Guide

本文件包含开发者在本仓库中常用的本地检查、Clippy 与工具链设置说明。

## Clippy & 项目配置
本项目在 crate 根使用了 Clippy 警告（`#![warn(clippy::all)]` 和 `#![warn(clippy::nursery)]`），并包含 `clippy.toml`（`msrv = "1.89.0"`）。为避免工具链不匹配，推荐使用 Rust 1.89.0 或更高版本。

## 设置工具链 (可选)
```powershell
rustup toolchain install 1.89.0
rustup override set 1.89.0
```

## 运行 Clippy (本地检查与自动修复)
```bash
cargo clippy --workspace --all-targets
cargo clippy --workspace --all-targets --fix
cargo clippy --workspace --all-targets --fix --tests
```

## 构建与测试
```bash
cargo build --workspace
cargo test --workspace
```

## 回归测试清单（发送/接收）
每次修改 `src/core/sender.rs` 或 `src/core/receiver.rs` 后，至少确认：

- 路径安全：
- 接收端拒绝路径穿越（`..`、空段、分隔符注入）。
- 导出目标存在冲突文件时，返回错误而不是覆盖。
- 流结束语义：
- 下载流未收到 `Done` 时必须报错，不能静默成功。
- 本地已完整对象分支不能 `panic`（不得使用不安全 `unwrap` 假设）。
- 事件顺序：
- 至少保证 `Started -> Progress -> Completed/Failed` 的生命周期语义清晰。
- 资源清理：
- 失败或中断后，临时目录（`.sendmer-recv-*`）应被清理。

建议执行：

```bash
cargo test -q
cargo clippy --all-targets --all-features -- -D warnings
```

## 关键流程说明

- `sender::send`：
- 校验输入路径，准备 endpoint/store，导入文件后生成 ticket。
- 发送侧进度由 provider 事件流驱动，不参与主错误控制流。

- `receiver::receive`：
- 根据 ticket 建立连接，先下载缺失 blob，再导出到目标目录。
- 导出阶段若遇冲突路径必须失败，并执行临时目录清理。
- 下载阶段若流异常提前结束（无 `Done`）必须返回错误。

## 提交与推送
在本地确认 lint 与测试通过后提交并推送：
```bash
git add -A
git commit -m "chore: apply clippy fixes / update README"
git push origin <branch>
```
