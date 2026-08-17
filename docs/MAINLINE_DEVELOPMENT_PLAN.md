# 主线开发计划（sendmer）

> 本文保留 M1-M3 的完成记录。`v0.6.0` 之后的前瞻计划、版本边界和验收门槛见 [FUTURE_DEVELOPMENT_PLAN.md](FUTURE_DEVELOPMENT_PLAN.md)。

## Context

`v0.6.0` 已建立可靠接收基线：send/receive 主流程、失败清理、路径安全、原子导出、重连重试、超时控制、并发接收、事件通知和 GitHub Release 门禁均已落地。后续主线应先稳定库 API 和可观测性，再探索持久化会话、安全增强和更大规模传输，避免在同一版本同时引入协议、GUI 和服务端架构。

## 当前进度（2026-08-18）

- M1 的默认输出目录、失败事件和临时目录清理已完成，并有 CLI/单元回归覆盖。
- M2 已完成：重试参数边界、每次尝试重新建连、symlink/containment 防护、endpoint 关闭和失败后的临时目录清理均已落地并通过门禁。
- 发送端 shutdown 现在先释放 router、进度任务和 blob store，再删除临时目录，并有真实启停回归覆盖 Windows 文件锁顺序。
- 发送端 shutdown 不再用短超时打断 Iroh 的优雅关闭，确保 endpoint 关闭和句柄释放完成后才删除临时 blob store。
- 发送端在 endpoint 在线等待期间收到 Ctrl+C 时会先经由 `SendResult::shutdown` 释放 router/store，再删除临时目录，避免 Windows 文件句柄竞争。
- 单个接收方中止或完成不会产生 sender 会话终态，也不会让 CLI 提前关闭共享；发送端仍按文档保持运行到 Ctrl+C。
- 发送端初始化导入失败也会清理临时 blob store，且有缺失源路径的确定性回归覆盖。
- 发送端会在创建 endpoint 与临时 blob store 前解析源路径，缺失或不可访问的输入直接失败，避免无效请求占用网络和临时资源。
- 接收端导出进度流只有收到 `Done` 才会成功；底层流提前结束会进入既有失败清理路径，避免将半导出误报为完成。
- M3 已完成首批门禁：release workflow 已加入版本一致性校验、`cargo check`、`cargo package`、锁定依赖构建和压缩包构建元数据，开发者指南已补充发布前 checklist，接收失败日志已保留阶段上下文。
- M3 已补充 release 资产验收：打包步骤检查二进制、元数据和最终压缩包，并要求发布 tag 已存在后再生成 release notes。
- push 与 workflow_dispatch 的 quality-gate/create/build/publish job 现在统一 checkout 已验证的 tag，构建元数据记录真实 tag commit。
- 已加入 `scripts/rehearse-release.ps1`：在临时 worktree 中校验本地/远端 tag，并执行与 release workflow 相同的 fmt、clippy、check、package、test 门禁，不上传资产。
- release workflow 的 `workflow_dispatch` 版本输入现在通过环境变量传递并先做严格 semver 校验，避免未校验字符串进入 shell。
- workflow、Unix 安装器和 PowerShell 安装器统一接受 `vMAJOR.MINOR.PATCH[-prerelease][+build]`，拒绝点分隔的伪 prerelease；CI 以有效/无效版本矩阵约束 workflow 与 Unix 安装器的规则，避免发布与安装阶段的 tag 规则分叉。
- `publish-crate` 现在先查询 crates.io：已存在的不可变版本跳过发布，查询异常则 fail closed，支持 release 重试幂等化。
- CI 新增固定版本 `actionlint` job，持续检查 `.github/workflows/` 的语义和表达式，降低 release workflow 回归风险。
- release workflow 新增取消保护的汇总 job，将 tag、运行链接和各阶段结果写入 GitHub Step Summary，失败时能快速定位阶段。
- release 资产现在同时上传 `.sha256` sidecar，Windows 与 Unix 安装器会在解压前校验 SHA-256 和资产文件名；Unix 端会归一化 hash 大小写以兼容标准 sidecar 格式。
- Bash 安装器现在也正确支持 MINGW/MSYS/Cygwin：选择 Windows ZIP 资产、校验 sidecar 并解压 `sendmer.exe`，避免请求不存在的 Windows tarball。
- PowerShell 安装器现在为每次安装使用唯一临时目录，并通过 `finally` 清理下载产物；CI 用模拟 checksum 下载失败覆盖原始错误保留和失败后无残留。
- M3 已完成 `v0.5.0` 的本地不上传 tag/release 演练（`scripts/rehearse-release.ps1 -Tag v0.5.0 -RequireRemoteTag`）：临时 worktree 中的 fmt、clippy、check、package、test 全部通过，且演练自动清理 worktree；GitHub Actions 的无上传 `workflow_dispatch` 演练也已成功，质量门禁和五个目标的构建/打包均通过，Release 创建、资产上传和 crate 发布均被跳过（[run 31315956210](https://github.com/bruceblink/sendmer/actions/runs/31315956210)）。
- `v0.6.0` 已完成原子 staging 导出和 native no-replace 提交：失败不会留下半目标，已有文件、目录或符号链接保持不变。
- `v0.6.0` 已完成下载阶段重连重试和可选连接、元数据、下载空闲超时；未设置 timeout 时保持既有行为，无效 timeout 在创建 endpoint 和临时目录前失败。
- `v0.6.0` 固化 fail-only 冲突策略，并在发送端拒绝空目录、空子目录和符号链接，避免当前 file-only collection 格式静默丢失数据。
- v0.7 API 首批已完成：`SendHandle` 隐藏 Router/FsStore/临时目录字段，CLI 支持基础 JSON Lines 事件；兼容的旧 API 继续保留，GUI 迁移使用 crates.io 版本号而不是 Git 提交依赖。
- `v0.7.0` 已发布：发送端共享上传限速、receive 取消、opaque 生命周期和基础 JSON 事件均已进入正式版本；AlterSendmer 已通过 `sendmer = "0.7.0"` 接入。
- `v0.8.0` 已完成版本化事件信封、严格序号、单终态、结构化错误和 stdout JSONL 契约，并有 fixture、多接收方、取消、失败和管道测试。
- AlterSendmer `v0.4.0` 已使用正式的 `sendmer = "0.8.0"` 完成阶段、结构化错误与历史记录迁移，并通过三平台发布门禁。
- sendmer `v0.9.0` 的持久 cache 与跨进程续传契约已冻结在
  [PERSISTENT_RECEIVE_CACHE_DESIGN.md](PERSISTENT_RECEIVE_CACHE_DESIGN.md)：默认临时缓存、显式启用、
  单条目排他锁、失败保留、成功删除、TTL 清理和版本化 manifest 均已明确。
- `v0.9.0` 第一阶段已完成：`ReceiveCacheOptions`、`--cache-dir`、`--cache-ttl-seconds`、跨平台
  advisory lock、schema v1 manifest、失败保留与成功删除均已实现；Windows 回归已验证关闭
  `FsStore` 后解锁再删目录。下一批实现显式/自动 prune 与缓存诊断；在真实独立进程中断恢复和
  弱网 E2E 完成前不发布版本。

## Recommended approach

### 里程碑 M1（1~2 周）：稳定性与行为一致性收敛

目标：把运行时行为、文档描述、失败路径事件统一，降低“能跑但不可观测”的问题。

范围：

- 统一 receive 默认输出目录语义（CLI 与 core 一致，文档一致）
  - `src/bin/sendmer.rs`
  - `src/core/receiver.rs`
  - `README.md`
  - `README_ZH.md`
- 完善失败路径事件闭环（失败时确保 emit_failed/日志可追踪）
  - `src/core/receiver.rs`
  - `src/core/progress.rs`
- 补齐对应回归测试（失败分支 + 参数分支）
  - `tests/cli.rs`
  - `src/core/receiver.rs`（单测）

非目标：

- 不新增传输协议能力
- 不改动对外 CLI 命令结构

验收：

- `cargo test --locked --workspace --all-features --bins --tests --examples`
- `cargo clippy --locked --workspace --all-targets --all-features`
- 行为回归点：receive 未传 `--output-dir`、下载失败时事件/日志可见、冲突失败后临时目录无泄漏

---

### 里程碑 M2（1~2 周）：路径与传输边界健壮性

目标：进一步压缩路径边界/中断场景的风险面。

范围：

- 对齐 sender/receiver 路径策略与边界测试矩阵
  - `src/core/sender.rs`
  - `src/core/receiver.rs`
  - `tests/cli.rs`
- 校验 `ReceiveRetryPolicy` 的最小有效边界（至少一次 size-fetch、chunk size 非零），并在初始化网络和临时存储前失败
  - `src/core/options.rs`
  - `src/core/receiver.rs`
- 补中断与重试相关回归（Ctrl+C、瞬时连接失败重试后的清理）
  - `src/core/receiver.rs`
  - `src/bin/sendmer.rs`
  - `tests/cli.rs`
- 仅做小步修复，不引入新抽象层

非目标：

- 不做 UI 形态变化
- 不引入新的配置系统

验收：

- `cargo fmt --all -- --check`
- `cargo test --locked --workspace --all-features --bins --tests --examples`
- 行为回归点：路径 traversal/symlink 边界拒绝、中断后资源回收、重试失败后清理一致
- 行为回归点：无效重试策略在创建临时目录前被拒绝，零毫秒 backoff 仍可用于快速重试

---

### 里程碑 M3（1~2 周）：发布链路与可观测性工程化

目标：让 release 过程稳定可复现、问题可追溯。

范围：

- 加强 CI 与 release 流程的发布验收项（产物命名/发布说明/失败可定位）
  - `.github/workflows/ci.yml`
  - `.github/workflows/release.yml`
- 增加发布前 checklist 与回归脚本化（保持与现有命令一致）
  - `README.md`
  - `README_ZH.md`
  - `DEVELOPMENT.md`（若已存在并承载开发流程）
- 统一关键错误日志与分类，提升线上定位效率
  - `src/core/receiver.rs`
  - `src/core/results.rs`

非目标：

- 不引入外部监控平台依赖
- 不改发布渠道（仍以 GitHub Release 为主）

验收：

- `cargo fmt --all -- --check`
- `cargo clippy --locked --workspace --all-targets --all-features`
- `cargo test --locked --workspace --all-features --bins --tests --examples`
- `cargo check --workspace --all-features --bins`
- 标签发布演练：从 tag 到 release 资产上传链路可一次成功

## Existing code to reuse

- `src/core/receiver.rs`
  - `resolve_output_dir`
  - `get_export_path`
  - `validate_path_component`
  - `finalize_cleanup`
- `src/core/results.rs`
  - `normalize_sender_cleanup_result`
  - `finalize_sender_shutdown`
- `src/core/sender.rs`
  - `canonicalized_path_to_string`
  - provider progress 并发限制（Semaphore）
- `src/core/progress.rs`
  - `TransferEventEmitter`
  - `ProviderProgressTracker`
- `tests/cli.rs`
  - send/receive 端到端测试基座

## Critical files to modify

- `src/core/receiver.rs`
- `src/core/sender.rs`
- `src/core/results.rs`
- `src/core/progress.rs`
- `src/bin/sendmer.rs`
- `tests/cli.rs`
- `.github/workflows/ci.yml`
- `.github/workflows/release.yml`
- `README.md`
- `README_ZH.md`

## Verification

统一验证顺序（每个小功能提交前执行）：

1. `cargo test`
2. `cargo clippy --locked --workspace --all-targets --all-features`
3. `cargo fmt --all`
4. 提交代码

阶段收敛验证（里程碑完成时执行）：

1. `cargo fmt --all -- --check`
2. `cargo clippy --locked --workspace --all-targets --all-features`
3. `cargo test --locked --workspace --all-features --bins --tests --examples`
4. `cargo check --workspace --all-features --bins`

发布前最终检查：

- 本地工作区干净（`git status`）
- 版本号与文档版本一致（`Cargo.toml` / `Cargo.lock` / `README*.md`）
- release 工作流可执行（tag 触发 + 产物上传 + release 说明）
