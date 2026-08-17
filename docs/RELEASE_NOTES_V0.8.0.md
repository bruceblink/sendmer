# sendmer 0.8.0 Release Notes

## 🚀 新功能 / Features

- 新增带 schema 版本、随机会话 ID、严格事件序号、时间戳、角色和阶段的事件信封。
- Added a versioned event envelope with random session IDs, strict sequencing, timestamps, roles,
  and transfer phases.
- `--json-events` 现在将可管道处理的 JSON Lines 独占写入 `stdout`，人类可读文本写入
  `stderr`。
- `--json-events` now keeps machine-readable JSON Lines on `stdout` and human output on `stderr`.

## 🐛 问题修复 / Bug Fixes

- 单个接收方中断或完成不再错误地关闭仍可服务其他接收方的 sender 会话。
- A single receiver abort or completion no longer closes a sender session that can still serve peers.
- completed、failed、cancelled 现在互斥，每个会话最多发出一个终态。
- completed, failed, and cancelled are now mutually exclusive per session.

## 🔒 安全 / Security

- 失败事件改用稳定错误码、安全摘要和显式可重试属性，不再要求消费者解析内部错误字符串。
- Failure events now expose stable codes, safe summaries, and explicit retryability without leaking
  internal error chains.
- 事件契约明确禁止暴露完整 ticket、绝对路径、节点密钥和底层连接标识。
- The event contract explicitly excludes full tickets, absolute paths, node secrets, and transport IDs.

## 🧰 维护、文档与测试 / Maintenance, Docs & Tests

- 新增固定 JSON fixture、并发顺序、单终态、多接收方、取消和 JSONL 管道 contract tests。
- Added a fixed JSON fixture and contract coverage for concurrent ordering, one terminal event,
  multiple receivers, cancellation, and piped JSONL.
- 新增 v0.7.0 到 v0.8.0 迁移指南与可编译的 Rust 事件消费者示例。
- Added a v0.7.0-to-v0.8.0 migration guide and a buildable Rust event consumer example.

Full Changelog 由 release workflow 按 `v0.7.0...v0.8.0` 的实际提交范围生成。
