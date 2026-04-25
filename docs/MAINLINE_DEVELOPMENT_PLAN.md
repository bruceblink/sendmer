# 主线开发计划（sendmer）

## Context

你现在希望的是“主线开发计划”（持续 1~2 个版本迭代），而不是单次缺陷修复。当前仓库已具备可发布基线（send/receive 主流程、清理策略、路径校验、并发限流、CI 基础链路），主线应聚焦“稳定性与发布工程化增强”，在不引入大功能的前提下持续降低回归风险、提升发布确定性。

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
