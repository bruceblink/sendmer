# 传输速率自定义配置评估

## 结论

可以支持，但第一版应实现**发送端上传速率上限**，不应一开始同时实现发送端、接收端、每个连接和精确 QoS 四套语义。

当前依赖 `iroh-blobs 0.103.0` 已经提供了适合实现这一功能的 provider throttle hook：

- `EventMask::throttle` 可以设置为 `ThrottleMode::Intercept`。
- provider 会发出带有 `connection_id`、`request_id` 和 chunk `size` 的 `Throttle` 事件。
- 事件处理器完成等待后，通过 oneshot 返回允许继续发送。
- 官方示例也使用这一机制实现按 chunk 延迟和最大连接数控制：[iroh-blobs limit example](https://github.com/n0-computer/iroh-blobs/blob/v0.103.0/examples/limit.rs)。

因此，这不是必须修改传输协议的功能，适合做成独立的 v0.6.1 小版本功能。

## 当前代码状态

sendmer 现在在 `src/core/sender.rs` 中创建 provider `EventSender`，只监听连接和请求进度，throttle 默认为关闭。现有的并发限制只限制进度处理任务数量，不限制网络字节速率。

接收端的 `GetProgressItem` 更适合报告进度，不适合作为第一版精确下载限速入口。若在消费 stream 时简单 sleep，实际网络缓存、QUIC 拥塞控制和本地写盘仍可能让结果不稳定，所以接收端限速应延后到有明确 backpressure 设计之后。

## 推荐 API

### Rust API

```rust
pub struct SendOptions {
    // Other existing options remain unchanged.
    pub max_upload_rate_bytes_per_sec: Option<std::num::NonZeroU64>,
}
```

`None` 表示不启用限速。使用 `NonZeroU64` 让零值在 API 和 CLI 校验阶段都明确失败，而不是把“零速率”解释成永久阻塞。

### CLI

```text
sendmer send ./my-folder --max-upload-rate 10485760
```

第一版使用明确的 bytes-per-second 数字，避免 `10M`、`10MiB` 和 `10MB` 的歧义。人类可读后缀可以在参数稳定后追加，不应阻塞核心实现。

参数语义：

- 这是 sender 的**总上传上限**，所有接收方共享。
- 它限制 payload 发送节奏，不保证包含所有协议开销后的线路速率。
- 真实吞吐可能低于配置值，原因包括 relay、网络拥塞、对端读取速度和磁盘速度。
- 限速只影响网络发送，不影响本地文件导入速度。
- 未配置时保持当前性能和事件路径不变。

## 推荐实现方式

1. `SendOptions` 携带可选的非零速率。
2. 只有启用速率时，才将 `EventMask::throttle` 设置为 `ThrottleMode::Intercept`。
3. sender 创建一个共享的 pacing/token-bucket 状态，所有 throttle 请求共同使用它。
4. 每次收到 chunk size 后计算下一次允许发送的时间点，释放锁后再 sleep，不能在锁内等待。
5. 等待完成后回复 provider 的 oneshot，让该 chunk 继续发送。
6. Router、provider、limiter task 在取消和 shutdown 时一起释放，不能留下后台任务。

共享 limiter 是第一版的推荐选择。若未来需要每个接收方独立限速，再把 limiter 按 `connection_id` 或 transfer id 分组；不要默认让每个连接都拥有完整配额，否则接收方数量一多，总出口速率会失控。

## 测试计划

- 参数校验：缺省、正常值、零值、过大值和 CLI 错误提示。
- 单元测试：chunk size 到等待时间的计算，覆盖整数溢出和极小速率。
- 单接收方 E2E：配置上限后，传输耗时应达到合理下界，同时允许网络抖动造成更慢结果。
- 多接收方 E2E：验证共享上限，而不是每个接收方都获得完整上限。
- 关闭配置 E2E：无 `--max-upload-rate` 时行为与当前版本一致。
- 取消和 Ctrl+C：限速等待期间可以退出，Router、store 和临时目录仍然完成清理。
- 事件回归：限速不改变 `Started`、`Progress`、`Completed` 和 `Failed` 的生命周期语义。

时间测试不应只断言一个精确毫秒值。应使用足够大的 payload、较宽的上下界和独立的 limiter 计算测试，减少 CI 网络抖动造成的假失败。

## 暂不实现的部分

- `--max-download-rate`：需要明确 receiver 侧 backpressure，暂不通过简单 sleep 模拟。
- 每个 peer 独立速率：先验证全局上限的用户需求和公平性。
- 精确线路 QoS：QUIC、relay 和系统调度无法由应用层完全保证。
- 动态运行时调速：先提供启动时配置，运行中调节可以放到稳定 handle/API 之后。

## 风险与决策门

实现前需要确定三个产品语义：

1. 速率单位是否只采用 bytes/sec，还是同时支持人类可读后缀。
2. 默认是否始终采用全局共享上限。
3. 速率上限是否只针对 payload，还是需要把协议开销纳入近似统计。

推荐答案是：第一版只支持 bytes/sec、全局共享、payload 上限。这样实现简单、可测试，也不会向用户承诺网络层无法保证的精度。
