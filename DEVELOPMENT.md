# 开发者指南 / Developer Guide

本文件包含开发者在本仓库中常用的本地检查、Clippy 与工具链设置说明。

架构、稳定接口约定和主线迭代计划统一见：[docs/MAINLINE.md](docs/MAINLINE.md)。核心模块拆分记录也在该文件中。

## 当前模块边界

核心传输代码按以下职责组织。

- `src/core/transport` 负责 iroh endpoint、地址和 relay 入口。
- `src/core/sender` 负责发送准备、导入、provider 控制和 sender 生命周期。
- `src/core/receiver` 负责接收准备、下载重试、恢复和接收终态。
- `src/core/manifest` 负责 TM1 清单模型、路径编码和清单校验。
- `src/core/filesystem` 负责 staging、路径安全、冲突检查和原子提交。
- `src/core/receive_cache` 和 `src/core/recovery` 负责缓存生命周期和恢复快照。
- `src/core/events` 和 `src/core/progress` 负责事件 schema、顺序校验、进度和终态。

sender 和 receiver 的内部实现使用私有子模块；原有公开入口由各自的 `mod.rs` 显式重导出。
内部跨模块调用使用明确的 `use` 和最小必要的 `pub(super)`，不新增 crate 对外接口。
sender 的 lifecycle 调用 setup/validation，setup 调用 import/provider/validation；
receiver 的 entry 调用 context/download/cleanup，download 调用 retry，retry 调用
prepare/policy/limits；错误分类及关闭原语独立于入口。CLI 和弱网测试保留原集成测试目标，
但测试完整限定名包含子模块名；原有按函数名子串筛选的命令继续有效。
真实子模块的格式检查统一由 `cargo fmt --all -- --check` 执行。

单个代码文件不得超过 600 行。单个函数不得超过 120 行。核心模块使用同一事件 schema
和相同的文件提交规则，避免 CLI 和桌面客户端复制传输实现。

## 构建模式

正式发布二进制显式使用 `--all-features`，包含旧版本的全部 CLI 能力。
源码默认启用 `cli`；最小库构建使用 `--no-default-features`。独立 feature 为
`manifest`、`persistent-recovery` 和兼容的 `clipboard`（依赖 cli）。
不改变基础事件 API、传输协议和路径安全校验。禁用的能力明确返回错误。

```bash
cargo clippy --locked --all-targets --no-default-features -- -D warnings
cargo test --locked --no-default-features --lib --test features
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --workspace --bins --tests --examples
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features --bins --tests --examples
```

## 代码大小检查

CI 和 Release Quality Gate 自动检查 Git 跟踪及新增、未忽略的 Rust、Shell、
PowerShell 和 Python 文件。文件按物理行数计数（包含空行和注释）；函数从定义签名
开始至函数体结束，包含空行和注释，不包含前置属性或装饰器。命名函数及嵌套函数
均检查，解析失败也会阻止通过。Markdown 和 workflow YAML 属于文档及配置，不计入代码文件。

本地复现（在仓库外的 Python 虚拟环境中安装固定版本的语法解析器）：

```bash
python -m pip install -r scripts/code-size-requirements.txt
python -B tests/check-code-size.py
python scripts/check-code-size.py
```

## Clippy & 项目配置
本项目在 crate 根使用了 Clippy 警告（`#![warn(clippy::all)]` 和 `#![warn(clippy::nursery)]`），并包含 `clippy.toml`（`msrv = "1.91.0"`）。为避免工具链不匹配，推荐使用 Rust 1.91 或更高版本。

## 设置工具链 (可选)
```powershell
rustup toolchain install 1.91.0
rustup override set 1.91.0
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
每次修改 `src/core/sender`、`src/core/receiver`、`src/core/filesystem` 或
`src/core/receive_cache` 后，至少确认：

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

## Windows 原生弱网验收

该验收使用 Windows 原生进程和本机 UDP 转发器。测试会注入限速、丢包、固定延迟、抖动和
双向断网，并检查 BLAKE3、恢复首包、完成时间、应用层重试次数、事件顺序和缓存清理。
测试使用 proxy ticket 把 sender 和 receiver 的数据经过本机转发端口。它不代表 Wi-Fi/热点
切换、两台真实设备传输或断电持久性。

在 PowerShell 中确认 Rust stable、Cargo 和 Git 可用，然后执行：

```powershell
$env:SENDMER_WEAK_NETWORK_SMOKE = "1"
$env:IROH_FORCE_STAGING_RELAYS = "1"

cargo test --locked --all-features --test weak_network_smoke local_udp_weak_network_baseline -- --ignored --nocapture
cargo test --locked --all-features --test weak_network_smoke local_udp_latency_and_combined -- --ignored --nocapture
```

需要验证真实 relay 时继续执行：

```powershell
$env:SENDMER_RELAY_SMOKE = "1"

cargo test --locked --all-features --test relay_smoke relay_only_ticket_round_trips_a_file -- --ignored --nocapture
cargo test --locked --all-features --test weak_network_smoke throttled_relay_transfer_recovers_after_sender_restart -- --ignored --nocapture
```

验收记录应包含提交版本、Windows 版本、Rust 版本、目标工具链、测试命令、随机种子、链路参数、
下行和上行转发字节、丢包计数、断网丢弃计数、恢复首包时间、完成时间、重试次数、最终 BLAKE3
和缓存目录清理结果。测试失败时保留完整 stdout、stderr、RUST_LOG 和失败阶段。

## 关键流程说明

- `sendmer::send_handle`：
- 校验输入路径，准备 endpoint/store，导入文件后生成 ticket。
- 发送侧进度由 provider 事件流驱动，不参与主错误控制流。

- `sendmer::receive`：
- 根据 ticket 建立连接，先下载缺失 blob，再导出到目标目录。
- 导出阶段若遇冲突路径必须失败，并执行临时目录清理。
- 下载阶段若流异常提前结束（无 `Done`）必须返回错误。

## Git Hooks（提交前自动检查）
可安装本仓库提供的脚本，在 `git commit` 前自动执行：

- `cargo fmt --all -- --check`
- `cargo clippy --locked --workspace --all-targets --all-features`

安装方式：

```bash
bash scripts/install-git-hooks.sh
```

安装成功后，每次提交都会触发上述检查，任一失败将阻止提交。

## 发布前检查清单

在创建并推送 `v<version>` 标签前，按以下顺序确认：

- `Cargo.toml` 的 package 版本与准备创建的标签一致。
- `python scripts/check-code-size.py`（先安装上方检查依赖）。
- `go run github.com/rhysd/actionlint/cmd/actionlint@v1.7.7`
- `cargo fmt --all -- --check`
- `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`
- `cargo check --workspace --all-features --bins`
- `cargo test --locked --workspace --all-features --bins --tests --examples`
- `git status --short` 为空，且发布说明会包含本次变更和验证结果。

本地质量检查通过后，先创建本地标签并运行无上传演练；脚本会在临时 worktree 中校验标签版本并执行发布前检查。含代码大小检查的新标签需要先激活上文的 Python 环境：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/rehearse-release.ps1 -Tag v0.10.0
```

演练通过后推送标签；推送标签会触发 release workflow。随后可使用 `-RequireRemoteTag -SkipQualityGate` 确认远端标签仍指向同一提交。release workflow 会再次校验标签版本与 `Cargo.toml`，让 quality-gate/create/build/publish 全部使用该标签，并在构建产物上传前执行同一组发布前检查和 `.sha256` sidecar 校验。

## 提交与推送
在本地确认 lint 与测试通过后提交并推送：

```bash
git add -A
git commit -m "chore: apply clippy fixes / update README"
git push origin <branch>
```
