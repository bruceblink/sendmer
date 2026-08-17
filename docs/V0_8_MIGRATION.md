# v0.8.0 事件 API 迁移指南

## 1. 术语表与命名约定

| 规范名称 | English / 缩写 | 职责边界 | 不代表什么 |
| --- | --- | --- | --- |
| 事件信封 | Event Envelope | 一条可序列化事件的版本、会话、顺序、角色、阶段与载荷 | 不是传输协议数据包 |
| 事件载荷 | Event Payload | started、progress、file names 或终态的业务内容 | 不是完整错误链或票据 |
| 传输会话 | Transfer Session | 一次 send 或 receive 的应用层生命周期 | 不是 connection/request ID |
| 结构化错误 | Transfer Error | 稳定错误码、阶段、可重试属性与安全消息 | 不是供程序解析的英文错误字符串 |
| JSON Lines | JSONL | 每行一个完整事件信封的 CLI 输出格式 | 不是一个 JSON 数组 |
| 旧通知事件 | Legacy Event | v0.7.0 的扁平、无版本事件形状 | 不是 v0.8.0 稳定事件契约 |

Rust 类型使用 `UpperCamelCase`，JSON 字段和值使用 `snake_case`。本文的版本号是 sendmer
crate 版本；`schema_version` 是独立的事件 schema 版本。

## 2. 依赖升级

升级到 crates.io 正式版本，不使用本地 path 或 Git 提交依赖：

```toml
[dependencies]
sendmer = "0.8.0"
```

sendmer `0.x` 的次版本可能包含破坏性 API 变更，因此 `0.7` 调用方不会被 Cargo 自动升级到
`0.8`。

## 3. Rust 事件迁移

v0.7.0 直接匹配扁平事件：

```rust,ignore
match event {
    TransferEvent::Progress { processed, total, speed, .. } => { /* ... */ }
    TransferEvent::Failed { message, .. } => { /* ... */ }
    _ => {}
}
```

v0.8.0 的 `TransferEvent` 是 `TransferEventEnvelope` 的公开别名。消费者先读取信封，再匹配
`event.event`：

```rust
use sendmer::{TransferEvent, TransferEventData};

fn consume(event: &TransferEvent) {
    match &event.event {
        TransferEventData::Progress {
            processed,
            total,
            speed_bytes_per_sec,
        } => println!("{processed}/{total} at {speed_bytes_per_sec} B/s"),
        TransferEventData::Failed { error } => {
            println!("{:?}: {}", error.code, error.message);
        }
        _ => {}
    }
}
```

错误码是枚举而不是字符串；实际代码应直接匹配 `TransferErrorCode`。上例中的展示逻辑应写成：

```rust
use sendmer::{TransferErrorCode, TransferEvent, TransferEventData};

fn may_retry(event: &TransferEvent) -> bool {
    matches!(
        &event.event,
        TransferEventData::Failed { error }
            if error.retryable
                && matches!(
                    error.code,
                    TransferErrorCode::ConnectionFailed
                        | TransferErrorCode::Timeout
                        | TransferErrorCode::TransferInterrupted
                )
    )
}
```

`EventEmitter::emit` 的参数相应升级为 `&TransferEventEnvelope`。完整可编译适配器见
[`examples/event_consumer.rs`](../examples/event_consumer.rs)。

`LegacyTransferEvent` 只用于短期迁移旧 JSON 数据，已弃用；新代码不得继续生产该格式。

## 4. 生命周期规则

每个事件消费者必须按以下规则更新状态：

1. 仅在 `started` 建立新的活动会话，首个 `sequence` 必须为 `1`。
2. 同一 `session_id` 只接受严格大于已处理值的 `sequence`。
3. completed、failed、cancelled 互斥；收到一个终态后丢弃该会话的迟到事件。
4. 新 `session_id` 启动后，旧会话事件不得覆盖当前 UI。
5. sender 的单个 provider request 完成或中断不是共享发送会话终态。
6. `SendHandle::close` 正常完成会话，`SendHandle::cancel` 取消会话。

消费者必须使用 `sequence` 排序，不能使用 `timestamp_ms` 决定同一会话内的先后。

## 5. CLI JSONL 迁移

`--json-events` 现在把事件信封逐行写入并刷新到 `stdout`。日志、票据提示和人类可读结果写入
`stderr`，因此管道只会收到 JSON：

```bash
sendmer receive --json-events <ticket> | jq -c \
  'select(.event.type == "progress") | {session_id, sequence, phase, progress: .event}'
```

v0.7.0 的扁平行：

```json
{"type":"progress","role":"receiver","processed":512,"total":1024,"speed":256.0}
```

v0.8.0 的版本化行：

```json
{"schema_version":1,"session_id":"0123456789abcdef0123456789abcdef","sequence":3,"timestamp_ms":1786982400000,"role":"receiver","phase":"transferring","event":{"type":"progress","processed":512,"total":1024,"speed_bytes_per_sec":256.0}}
```

脚本应拒绝未知的必需 `schema_version`，但应忽略信封和载荷中的未知可选字段。

## 6. 错误与隐私

- 使用 `error.code` 驱动本地化摘要，不解析 `error.message`。
- 仅在 `error.retryable` 为 `true` 时提供自动重试入口。
- 使用 `error.phase` 展示失败阶段；不要从事件类型反推阶段。
- 诊断日志不得记录完整 ticket、绝对路径、节点密钥或底层连接 ID。
- `session_id` 只用于关联应用层事件，不能替代内容 hash 或访问控制。

完整字段、错误码和隐私边界见
[`MAINLINE.md`](MAINLINE.md#44-事件信封与结构化错误)。
