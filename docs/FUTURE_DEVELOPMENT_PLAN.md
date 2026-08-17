# 后续开发计划（v0.8.0 之后）

## 1. 术语表与命名约定

| 规范名称 | English / 缩写 | 职责边界 | 不代表什么 |
| --- | --- | --- | --- |
| 核心传输层 | sendmer Core | 负责 ticket、点对点传输、限速、重试、路径安全和资源清理 | 不负责桌面界面、账号或云端文件托管 |
| 桌面客户端 | AlterSendmer Desktop | 负责 GPUI 交互、配置、状态展示、历史和平台集成 | 不复制传输协议或资源清理逻辑 |
| 传输会话 | Transfer Session | 一次 send 或 receive 操作的应用层生命周期 | 不等同于单条 QUIC 连接或 provider request |
| 事件信封 | Event Envelope | 带 schema 版本、会话标识、顺序和阶段的稳定事件结构 | 不参与传输控制流，也不替代函数返回值 |
| 结构化错误 | Transfer Error | 提供错误码、失败阶段、可重试属性和展示消息 | 不等同于本地化 UI 文案或底层错误字符串 |
| 传输票据 | Ticket | 允许接收方连接和请求内容的 bearer capability | 不是账号、长期授权或云端分享链接 |

本文后续统一使用上述规范名称。AlterSendmer 只能通过核心传输层的公开 API 消费能力；
内部 iroh 类型、临时存储和 Router 生命周期不得泄漏到桌面客户端。

## 2. 当前基线与产品边界

当前发布基线是 `v0.8.0`。项目已经具备：

- 基于 iroh 的文件和目录点对点传输，以及 CLI 和可复用 Rust API。
- 原子接收导出、no-replace 冲突策略、重试、超时、取消和失败清理。
- `SendHandle` opaque 生命周期 API 和兼容用的 `SendResult`。
- sender 全局共享的 payload 上传速率上限。
- 带会话、严格序号、阶段、单终态与结构化错误的版本化 JSON Lines 事件。
- Linux、macOS、Windows 的测试、安装器和 GitHub Release 链路。
- AlterSendmer 下一版本将通过 crates.io 上的 `sendmer = "0.8.0"` 消费新事件契约。

主产品边界仍是“隐私优先的一次性文件传输”。核心传输层近期不引入账号系统、云端
文件存储、自建控制面或后台同步服务。传输票据继续采用 bearer capability 语义：取得票据
即可使用，因此 UI 和文档必须提醒用户只通过可信渠道分享。

## 3. 版本路线

### v0.6.0：可靠接收与数据完整性（已完成）

本版本完成原子 staging 导出、目标 no-replace 提交、下载阶段重连重试、单调进度、连接/
元数据/下载空闲超时，以及失败后的临时目录清理。

冻结的安全契约：

- 冲突策略仅支持 `fail`；已有文件、目录或符号链接绝不覆盖、合并或重命名。
- 单次 receive 会复用临时 store 中已获得的数据，但不承诺跨进程断点续传。
- collection 只表示常规文件；发送端拒绝空目录、空子目录和符号链接。
- 多顶层根 collection 会被拒绝，避免无法原子提交的部分结果。

### v0.7.0：嵌入式 API、取消与发送限速（已完成）

本版本已经发布并完成：

- `SendHandle` 隐藏 Router、FsStore、TempTag 和临时目录字段。
- 明确的 `cancel`、`close`、`status` 生命周期与旧 API 兼容层。
- receive watch cancellation 和退出时有序清理。
- `SendOptions::max_upload_rate_bytes_per_sec` 与 CLI `--max-upload-rate`。
- sender 的所有接收方共享同一 payload 上传上限。
- `TransferEvent` 的 Serde 支持和 `--json-events` JSON Lines 输出。
- AlterSendmer 使用正式版本依赖接入 `SendHandle`，不依赖本地路径或 Git 提交。

上传限速的已实现语义和剩余测试见
[TRANSFER_RATE_LIMIT_DESIGN.md](TRANSFER_RATE_LIMIT_DESIGN.md)。当前 JSON 事件仍是基础通知模型，
不包含稳定的会话标识、阶段、事件序号或结构化错误；这些能力属于 `v0.8.0`。

### AlterSendmer v0.3.0：上传速率配置闭环

桌面客户端先消费核心传输层已经发布的能力，不修改协议：

- 提供“无限制”或自定义 MiB/s 的发送端上传上限。
- 对用户输入执行非零、范围和整数溢出校验，再转换为 bytes/s。
- 持久化配置，并在设置面板显示当前有效值。
- 将配置传给 `SendOptions`，不在桌面客户端实现第二套 limiter。
- 增加配置映射、持久化、传输适配和视觉验收。

AlterSendmer `v0.3.0` 依赖 `sendmer = "0.7.0"`，已经完成 `SendHandle`、上传限速和
release changelog CI 接入。事件信封迁移统一进入 `v0.4.0`，不制造中间依赖版本。

### v0.8.0：版本化事件与结构化错误契约（已完成）

周期：1 至 2 周。目标是让桌面客户端、脚本和其他 Rust 服务可靠消费传输状态。
字段、阶段、错误码、隐私边界和迁移顺序见
[TRANSFER_EVENT_SCHEMA.md](TRANSFER_EVENT_SCHEMA.md)。

- 引入版本化事件信封，至少包含 schema 版本、传输会话 ID、事件序号、时间戳、角色和阶段。
- 区分 started、progress、file names、completed、failed 和 cancelled，保证每个会话仅有一个终态。
- 引入公开的结构化错误，提供错误码、失败阶段、可重试属性和安全展示消息。
- 默认不在事件中暴露完整传输票据、绝对路径或底层连接密钥。
- sender 多接收方仍以一个传输会话聚合；底层 connection/request ID 不冒充应用层会话 ID。
- 提供 JSON schema fixture、事件顺序、取消、失败和多接收方 contract tests。
- 保留清晰的迁移文档；由于现有 `TransferEvent` 是可穷举枚举，结构变更使用新的次版本，
  不伪装成 `v0.7.1` 补丁。

验收门槛：`cargo doc --no-deps`、公开 API 示例、事件 fixture、MSRV 1.91 检查、跨平台 CI
和现有完整门禁全部通过。发布到 crates.io 后，AlterSendmer 才能把依赖升级为
`sendmer = "0.8.0"`；不得提交本地 path 或 Git 提交依赖作为过渡方案。

### AlterSendmer v0.4.0：消费 sendmer v0.8.0 契约

- 使用核心阶段驱动 UI，不再仅靠 started/progress 事件推断生命周期。
- 按错误码展示本地化摘要、可展开技术详情和是否可重试。
- 历史记录增加可选会话 ID、失败阶段和错误码，并兼容已有 `history.json`。
- 诊断信息保持隐私边界，不记录完整票据和绝对路径。
- 依赖正式发布的 `sendmer = "0.8.0"`，完成跨项目回归后发布 `v0.4.0`。

### v0.9.0：持久化、规模和安全

周期：2 至 4 周，只有 `v0.8.0` 的会话、事件和错误契约稳定后才启动：

- 可选持久 receive cache 和真正的跨进程断点续传。
- cache TTL、清理命令、锁和崩溃遗留目录回收。
- sender 会话过期、最大接收方数量和主动撤销。
- 带宽、并发和内存上限，以及大目录和大文件基准。
- 非 UTF-8 文件名、权限、时间戳和符号链接的跨平台策略。
- Release 资产签名、SBOM 和构建 provenance。
- ARM 设备、真实 relay 和弱网 smoke test。

## 4. 暂不纳入主线的方向

- 后台 daemon 或同步服务：它需要持久状态、冲突解决、认证和升级运维，属于第二个产品方向。
- 自建 relay、云端文件存储和多租户控制面：当前主线不承担对应运维成本。
- 默认覆盖、静默权限修改和不透明的票据共享：这些会扩大数据丢失和安全风险。
- 接收端应用层 sleep 限速：没有明确 backpressure 前，不把不稳定行为包装为下载限速。
- GUI 改写核心协议：桌面客户端只能消费稳定 API，不能成为协议语义的第二来源。

## 5. 质量、提交与发布顺序

每个小功能独立提交。核心传输层提交前至少执行：

```text
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features --bins --tests --examples
cargo check --workspace --all-features --bins
```

涉及 CLI、安装器或 workflow 时，再执行相应 actionlint、安装器测试和 release rehearsal。
AlterSendmer 必须执行 fmt、locked check、Clippy、workspace tests，以及受影响界面的 Windows
截图验收。成功提交后立即推送；版本 tag 只能指向已经通过全部发布门禁的提交。

跨项目发布顺序固定为：

1. 在 sendmer 完成功能、测试、文档和版本提交。
2. 发布 sendmer crate 与 GitHub Release，并确认 crates.io 可解析该版本。
3. AlterSendmer 使用该正式版本号升级依赖并完成跨项目回归。
4. 发布 AlterSendmer，不使用本地 path 或 Git 提交依赖绕过发布顺序。

## 6. 进入下一阶段的指标

AlterSendmer `v0.3.0` 发布前：

- 无限速与自定义速率均可持久化并正确映射到 `SendOptions`。
- 无效输入不会启动传输；限速不破坏取消和退出清理。
- 设置页在默认与最小窗口尺寸下无重叠，跨平台 CI 全绿。

sendmer `v0.8.0` 发布前：

- 事件 schema fixture 固定，所有事件具有同一会话 ID 和严格递增序号。
- completed、failed、cancelled 三种终态互斥且仅发出一次。
- 公开错误码和重试属性有 contract tests，日志与事件不泄漏敏感票据。
- 多接收方、取消和关闭语义有文档与可重复测试覆盖。

进入 `v0.9.0` 前：

- 大文件、弱网和跨平台构建具有可重复基准。
- cache 的所有权、TTL、锁、崩溃恢复和清理命令已有独立设计评审。
- 持久化格式具备版本字段和迁移策略，不依赖 GUI 私有状态。
