# 版本化传输事件契约

## 1. 术语表与命名约定

| 规范名称 | English / 缩写 | 职责边界 | 不代表什么 |
| --- | --- | --- | --- |
| 传输会话 | Transfer Session | 一次 send 或 receive 的应用层生命周期 | 不是 QUIC 连接、provider request 或传输票据 |
| 事件信封 | Event Envelope | 承载 schema 版本、会话标识、顺序、时间、角色、阶段和事件载荷 | 不参与传输控制流，也不替代函数返回值 |
| 事件载荷 | Event Payload | 描述 started、progress、file names 或终态的业务数据 | 不包含完整传输票据、绝对路径或连接密钥 |
| 会话标识 | Transfer Session ID | 关联同一次传输的随机应用层标识 | 不是底层 connection ID、request ID 或内容 hash |
| 事件序号 | Sequence Number | 在一个传输会话内从 1 开始严格递增的顺序 | 不是全局序号，也不是时间戳 |
| 传输阶段 | Transfer Phase | 表示事件发生时核心传输层正在执行的稳定阶段 | 不是 UI 页面、进度百分比或底层协议状态 |
| 结构化错误 | Transfer Error | 提供稳定错误码、失败阶段、可重试属性和安全消息 | 不是本地化 UI 文案或完整错误链 |
| 终态事件 | Terminal Event | completed、failed、cancelled 三种互斥结果之一 | 不是单个接收方断开或一次重试失败 |

Rust 类型使用 `UpperCamelCase`，JSON 字段和值使用 `snake_case`。本文首次出现使用
“中文规范名（English）”，后续统一使用中文规范名。

## 2. 目标与边界

`v0.8.0` 将现有通知型 `TransferEvent` 升级为事件信封，使桌面客户端、脚本和其他 Rust
服务不再依赖事件到达时机或错误字符串推断生命周期。

本契约只描述可观测状态，不改变传输协议、ticket 格式、重试策略和资源清理顺序。发送端
多个接收方继续聚合为一个传输会话；单个 provider request 的开始或中断不会创建新的会话，
也不会直接产生传输会话的终态。

## 3. 事件信封

公开 Rust 结构按以下字段冻结：

```rust
pub struct TransferEvent {
    pub schema_version: u16,
    pub session_id: TransferSessionId,
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub role: Role,
    pub phase: TransferPhase,
    pub event: TransferEventData,
}
```

- `schema_version`：首版固定为 `1`。它是 JSON schema 版本，不等于 sendmer crate 版本。
- `session_id`：128 位随机值编码为 32 个小写十六进制字符；不得从 ticket、内容 hash 或
  网络连接标识派生。
- `sequence`：每个会话从 `1` 开始，由同一个同步事件状态机分配，任何并发回调也必须保持
  严格递增。
- `timestamp_ms`：Unix epoch 毫秒，仅用于展示和跨进程关联；消费者必须使用 `sequence`
  判断同一会话内顺序。
- `role`：`sender` 或 `receiver`。
- `phase`：事件发生时的稳定传输阶段。
- `event`：带 `type` 判别字段的事件载荷。

进度事件示例：

```json
{
  "schema_version": 1,
  "session_id": "0123456789abcdef0123456789abcdef",
  "sequence": 3,
  "timestamp_ms": 1786982400000,
  "role": "receiver",
  "phase": "transferring",
  "event": {
    "type": "progress",
    "processed": 524288,
    "total": 1048576,
    "speed_bytes_per_sec": 262144.0
  }
}
```

## 4. 传输阶段

| JSON 值 | 规范名称 | 起止边界 |
| --- | --- | --- |
| `preparing` | 准备阶段（Preparing） | 输入校验、临时存储、endpoint 和发送集合准备 |
| `connecting` | 连接阶段（Connecting） | endpoint 上线、远端连接与重连 |
| `metadata` | 元数据阶段（Metadata） | collection、hash sequence、文件名和大小发现 |
| `transferring` | 数据阶段（Transferring） | payload 下载或发送进度 |
| `exporting` | 导出阶段（Exporting） | 接收端 staging、校验和 no-replace 提交 |
| `finalizing` | 收尾阶段（Finalizing） | endpoint/store 关闭、临时目录清理和成功返回 |

失败或取消事件保留发生时的阶段，不把 `failed`、`cancelled` 伪装成阶段。完成事件使用
`finalizing`，表示核心资源已按正常路径收口。

## 5. 事件载荷与终态

| `type` | 是否终态 | 载荷 |
| --- | --- | --- |
| `started` | 否 | 无额外字段 |
| `progress` | 否 | `processed`、`total`、`speed_bytes_per_sec` |
| `file_names` | 否 | 相对文件名数组 `file_names` |
| `completed` | 是 | 无额外字段 |
| `failed` | 是 | `error` 结构化错误 |
| `cancelled` | 是 | 无额外字段 |

每个传输会话必须满足：

1. 首个事件是 `started`，序号为 `1`。
2. 后续事件沿同一个会话标识严格递增，允许没有 progress 或 file names。
3. completed、failed、cancelled 互斥，且最多发出一个。
4. 终态之后的底层迟到回调不再对外发事件，只记录内部诊断日志。
5. 暂时重试失败和单个接收方中断不是传输会话终态。

## 6. 结构化错误

失败事件中的错误结构固定为：

```json
{
  "code": "connection_failed",
  "phase": "connecting",
  "retryable": true,
  "message": "unable to connect to the sender"
}
```

首版错误码集合：

| 错误码 | 使用边界 |
| --- | --- |
| `invalid_input` | ticket、路径或公开选项校验失败 |
| `connection_failed` | endpoint 上线、连接或远端不可达 |
| `timeout` | 连接、元数据或下载空闲超时 |
| `remote_rejected` | 远端明确拒绝或请求不被支持 |
| `transfer_interrupted` | 数据流在完成前中断且重试耗尽 |
| `target_conflict` | 最终输出目标已存在或 no-replace 提交冲突 |
| `filesystem` | 本地读取、写入、导出或清理失败 |
| `internal` | 无法安全归类的内部失败 |

`retryable` 由失败现场根据剩余重试和操作语义明确设置，消费者不得仅凭错误码自行猜测。
`message` 是安全的英文展示摘要；完整 `anyhow` 错误链只进入受控诊断日志，不进入稳定 JSON。

## 7. 隐私与安全

- 事件不得包含完整 ticket、私钥、节点密钥、relay token 或底层连接标识。
- 文件名事件只允许 collection 中的相对名称，不允许本机绝对路径。
- 会话标识使用独立随机值，不能据此恢复内容 hash 或网络身份。
- 未知内部错误统一降级为 `internal` 和安全摘要，不直接序列化 `Debug` 输出。

## 8. 兼容与实施顺序

`TransferEvent` 当前是公开可穷举枚举，因此事件信封作为 `v0.8.0` 的次版本变更发布；依赖
`sendmer = "0.7"` 的调用方不会被 Cargo 自动升级。`EventEmitter` 和 `AppHandle` 的职责保持
不变，但其事件参数升级为事件信封。迁移方应匹配 `event.event`，并使用会话标识和序号维护
状态，不能继续只看错误字符串。

实施按以下独立批次进行：

1. 添加公开类型、固定 JSON fixture、会话标识解析和 Serde contract tests。
2. 让发送/接收入口各创建一个事件状态机，接入严格序号和单终态约束。
3. 在取消、超时、连接、传输、冲突和文件系统失败现场构造结构化错误。
4. 更新 `--json-events`、公开 API 示例、迁移文档、MSRV 和跨平台门禁。

`v0.8.0` 发布到 crates.io 后，AlterSendmer 才能使用正式版本号迁移；不得以 Git revision
提前消费尚未冻结的事件契约。
