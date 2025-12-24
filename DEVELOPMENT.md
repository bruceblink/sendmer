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

## 提交与推送
在本地确认 lint 与测试通过后提交并推送：
```bash
git add -A
git commit -m "chore: apply clippy fixes / update README"
git push origin <branch>
```

如果你希望我现在为当前分支执行 `git push`，或继续清理剩余的 Clippy 警告，我可以继续操作。