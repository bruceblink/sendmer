# 后续开发计划（v0.6.0 之后）

## 1. 基线与产品边界

当前基线是 `v0.6.0`。项目已经具备：

- 基于 iroh 的文件和目录点对点传输。
- CLI 和可复用 Rust API 两种入口。
- relay、ticket、路径 containment、失败清理和多接收方基础支持。
- Linux、macOS、Windows 的测试和发布链路。
- GitHub Release、安装器校验、SHA-256 sidecar 和本地 release rehearsal。

当前推荐的产品边界仍是“隐私优先的一次性文件传输”。sendmer 不在近期引入账号系统、云端文件存储或自建控制面。GUI、Tauri 和其他 Rust 服务应先作为稳定库 API 的消费者，而不是先改变核心传输协议。

## 2. 版本路线

### v0.6.0：可靠接收与数据完整性（已完成）

本版本已完成原子 staging 导出、目标 no-replace 提交、下载阶段重连重试、进度不倒退、连接/元数据/下载空闲超时参数，以及失败后的临时目录清理。

本版冻结以下安全契约：

- 冲突策略仅支持 `fail`；已有文件、目录或符号链接绝不覆盖、合并或重命名。
- 单次 receive 会复用临时 store 中已获得的数据，但不承诺跨进程断点续传。
- 当前 collection 格式只表示常规文件；发送端会拒绝空目录、空子目录和符号链接，避免静默丢失目录元数据。
- 多顶层根 collection 会被拒绝，因为它们无法作为一个原子目标发布。

遗留的 Windows 文件锁和真实弱网故障注入回归会继续在 `v0.7.0` 的 contract/可靠性测试中扩展。

### v0.6.1：可配置发送速率

这是一个可以独立交付的小功能，建议放在 v0.6.0 的接收可靠性完成之后。详细评估见 [TRANSFER_RATE_LIMIT_DESIGN.md](TRANSFER_RATE_LIMIT_DESIGN.md)。

第一版只支持发送端的全局上传上限：

- `SendOptions` 增加可选的 bytes-per-second 上限。
- CLI 增加 `--max-upload-rate <BYTES_PER_SECOND>`。
- 一个 sender 的所有接收方共享同一个上限，避免多接收方把总出口带宽放大。
- 不承诺精确吞吐，只承诺尽量不超过配置上限。
- 不在第一版提供 `--max-download-rate`，也不把本地导入和磁盘写入混入网络限速。

验收重点：单接收方速率、并行接收方总速率、关闭限速时的行为、取消和 shutdown 清理。

### v0.7.0：稳定库 API 与可观测性

周期：1 至 2 周。目标是让 sendmer 可以被 GUI、脚本和其他 Rust 服务可靠嵌入。

- 为 `TransferEvent` 增加 `Serialize` 和稳定 schema。
- 增加 `transfer_id`、阶段、路径、错误码、时间戳、处理字节和总字节。
- 提供 JSON Lines 输出，便于脚本和 GUI 消费。
- 用 opaque `SendHandle` 隐藏 Router、FsStore、TempTag 等 iroh 内部字段。
- 提供明确的 `cancel`、`close`、`status` 生命周期契约，并为旧 API 提供兼容过渡。
- 引入结构化错误类型，减少上层依赖错误字符串。
- 增加多接收方、取消、失败和事件顺序的 API contract tests。

验收门槛：`cargo doc --no-deps`、公开 API 示例、事件 schema fixture、MSRV 1.91 检查和现有完整门禁全部通过。

### v0.8.0：持久化、规模和安全

周期：2 至 4 周，是否启动取决于 v0.6 的可靠性指标和 v0.7 的 API 稳定性。

- 可选持久 receive cache 和真正的断点续传。
- cache TTL、清理命令、锁和崩溃遗留目录回收。
- sender 会话过期、最大接收方数量、主动撤销和更明确的 bearer-ticket 警告。
- 带宽、并发和内存上限，大目录和大文件基准。
- 非 UTF-8 文件名、权限、时间戳和符号链接的跨平台策略。
- Release 资产签名、SBOM 和构建 provenance。
- ARM 设备、真实 relay 和弱网 smoke test。

## 3. 暂不纳入主线的方向

- GUI 或 Tauri 主界面：等稳定事件 API 后作为独立消费者开发。
- 后台 daemon 或同步服务：这会引入持久状态、冲突解决、认证和升级运维，属于第二个产品方向。
- 自建 relay、云端文件存储和多租户控制面：当前 iroh relay 已满足主线需求，不应提前承担运维成本。
- 默认覆盖、静默权限修改和不透明的 ticket 共享：这些会扩大数据丢失和安全风险。

## 4. 质量和提交规则

每个小功能单独提交，提交前至少执行：

```text
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features --bins --tests --examples
cargo check --workspace --all-features --bins
```

涉及 CLI、安装器或 workflow 时，再执行：

```text
go run github.com/rhysd/actionlint/cmd/actionlint@v1.7.7
bash ./tests/release-version.sh
bash ./tests/install-target.sh
powershell -NoProfile -ExecutionPolicy Bypass -File tests/install.ps1
```

里程碑完成后使用 `scripts/rehearse-release.ps1` 在临时 worktree 中复现发布门禁。成功提交后立即推送当前分支；发布 tag 必须指向已经通过质量门的提交。

## 5. 进入下一阶段的指标

v0.6.0 进入 v0.7.0 前，至少要有：

- 导出失败后的 partial 目标残留数为零。
- 接收临时目录泄漏回归为零。
- 真实下载重试和取消测试稳定通过。
- 并行接收方不会互相错误终止。

v0.7.0 进入 v0.8.0 前，至少要有：

- 事件 schema 和公开 API contract 固定。
- 多接收方状态、取消和关闭语义有文档和测试覆盖。
- 大文件、弱网和跨平台构建有可重复基准。
