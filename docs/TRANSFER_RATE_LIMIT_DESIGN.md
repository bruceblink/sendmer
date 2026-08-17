# 传输速率自定义配置设计与实现状态

## 1. 术语表与命名约定

| 规范名称 | English / 缩写 | 本文含义 | 不代表什么 |
| --- | --- | --- | --- |
| 上传速率上限 | Upload Rate Limit | 一个 sender 对所有接收方共享的 payload bytes/s 上限 | 不是精确线路 QoS |
| 限速器 | Upload Rate Limiter | 核心传输层内按 chunk 安排发送时间的共享状态 | 不是 QUIC transport window |
| payload | Payload | iroh-blobs 传输的文件内容字节 | 不包含所有 QUIC、relay 和 Bao 协议开销 |

## 2. 结论与当前状态

发送端上传速率上限已经在 sendmer `v0.7.0` 实现，不需要修改传输协议或 fork iroh。
当前依赖 `iroh-blobs 0.103.0` 的 provider throttle hook：

- 启用配置时将 `EventMask::throttle` 设置为 `ThrottleMode::Intercept`。
- provider 发送包含 `connection_id`、`request_id` 和 chunk `size` 的 `Throttle` 事件。
- 核心传输层等待共享限速器分配的时刻，再回复 oneshot 允许继续发送。
- 未配置上限时不启用 throttle 事件，保持原有发送路径。

官方机制参考：[iroh-blobs limit example](https://github.com/n0-computer/iroh-blobs/blob/v0.103.0/examples/limit.rs)。

## 3. 已发布 API

### Rust API

```rust
pub struct SendOptions {
    // Other existing options remain unchanged.
    pub max_upload_rate_bytes_per_sec: Option<std::num::NonZeroU64>,
}
```

`None` 表示无限制。`NonZeroU64` 让零值在 API 边界明确失败，而不是把零解释成永久阻塞。

### CLI

```text
sendmer send ./my-folder --max-upload-rate 10485760
```

CLI 使用明确的 bytes/s 整数，避免 `10M`、`10MiB` 和 `10MB` 的歧义。AlterSendmer 可以在
界面中接收 MiB/s，再经过校验和 checked multiplication 转换为该 API 所需的 bytes/s。

## 4. 已实现语义

- 上限属于一个 sender，所有接收方共享，接收方增加不会复制完整配额。
- 上限控制 payload 发送节奏，不保证把协议开销计算在内。
- 真实吞吐可以低于配置值，原因包括 relay、拥塞、对端读取速度和磁盘速度。
- 限速不影响本地文件导入和接收端写盘。
- 限速器在锁内只预留时间，不在锁内 sleep；等待不会串行阻塞事件处理锁。
- Router、provider、限速任务和临时目录仍遵循 `SendHandle` 的取消与关闭顺序。

## 5. 已有验证

- CLI 参数测试覆盖缺省、非零值和零值拒绝。
- provider event mask 测试确认只有配置上限时启用 throttle。
- 限速计算测试覆盖向上取整，避免因为整数截断超过配置上限。
- 共享调度单元测试确认多个请求使用同一发送时间线。
- 单接收方 CLI E2E 使用真实本地传输验证耗时下界。
- 完整 fmt、Clippy、workspace tests 和跨平台 CI 已随 `v0.7.0` 通过。

时间测试只断言合理下界，不依赖精确毫秒值，以减少 CI 调度抖动造成的假失败。

## 6. 后续验证与非目标

后续仍应增加：

- 两个真实并行接收方共享总上限的 E2E。
- 限速等待期间取消和应用退出的专门回归。
- 大文件、relay 和弱网环境下的吞吐基准。

当前非目标：

- `--max-download-rate`：接收端没有明确 backpressure 设计前，不通过简单 sleep 模拟。
- 每个 peer 独立配额：它会改变“sender 总出口上限”的现有契约。
- 精确线路 QoS：应用层无法完全控制 QUIC、relay 和系统调度开销。
- 运行中动态调速：`v0.7.0` 只接受启动时配置；动态更新需先设计稳定 handle API。

因此，AlterSendmer `v0.3.0` 应只负责采集、校验、持久化和传递配置，不在 GUI 中实现
第二套限速器。核心传输层仍是上传速率语义的唯一来源。
