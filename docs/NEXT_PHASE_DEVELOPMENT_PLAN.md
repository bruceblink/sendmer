# sendmer 下一阶段开发计划：v0.10.0 发布后的桌面适配

> 文档状态：已完成执行记录，基线日期：2026-08-25。<br>
> 适用范围：`F:\project\sendmer` 及其正式桌面消费者 `F:\project\alter-sendme-gpui`。<br>
> 计划边界：本文件把下一阶段拆成可验收的跨项目批次，不修改 `sendmer v0.10.0` 已冻结的核心协议；稳定契约仍以 [MAINLINE.md](MAINLINE.md) 为准。

## 1. 术语表与命名约定

| 规范名称 | English / Acronym | 当前计划中的职责边界 | 不代表什么 |
| --- | --- | --- | --- |
| 核心传输层 | sendmer Core | 提供 Rust crate、CLI、ticket、点对点传输、重试、缓存和资源清理 | 不负责 GPUI 界面、账号、云端文件托管或桌面历史 |
| 桌面客户端 | AlterSendmer Desktop | 通过 crates.io 上的公开 sendmer API 提供 GPUI 交互、配置、历史和平台集成 | 不复制 iroh、缓存布局、manifest 解析或限速实现 |
| 传输适配器 | Transfer Adapter | 把桌面偏好、取消信号和事件映射到公开 API | 不是第二套传输协议或隐藏的私有 API 访问层 |
| 公开接口约定 | Public API Contract | `SendOptions`、`ReceiveOptions`、`SendHandle`、事件信封和结构化错误等稳定接口 | 不包括 `sendmer` 的私有模块、临时目录、Router 或底层连接对象 |
| 传输清单 | Transfer Manifest / TM1 | `--manifest` 或 `SendOptions::manifest_mode` 携带空目录和受支持元数据 | 不是接收缓存的本地 `manifest.json` |
| 原生验收 | Native Acceptance | 在真实 Windows、Linux 或 macOS 环境检查窗口、文件系统和打包行为 | 不是编译通过、单元测试或静态截图替代品 |
| 发布批次 | Release Batch | 一组完成测试、文档、版本和发布证据后独立提交的变更 | 不是把多个未验证功能合并提交 |

正文、表格和代码中的组件名均使用以上名称；标准协议名 `QUIC`、`TLS`、`JSON Lines` 和
`SHA-256` 保留标准大小写。

## 2. 当前基线与判断

### 2.1 已完成事实

| 项目 | 当前结果 | 本次核对依据 |
| --- | --- | --- |
| 核心版本 | `sendmer 0.10.0`，`Cargo.toml` 与 `Cargo.lock` 一致 | [Cargo.toml](../Cargo.toml)、[Cargo.lock](../Cargo.lock) |
| 核心发布 | tag `v0.10.0` 已发布，tag 指向 `fe1f475`；`main` 文档收尾提交为 `37a932e` | `git tag`、`git ls-remote origin`、[MAINLINE.md](MAINLINE.md) |
| v0.10 范围 | sender 会话生命周期、接收方/文件/总大小/导入工作集限制、TM1 manifest、供应链证明和六平台发布矩阵已完成 | [MAINLINE.md](MAINLINE.md)、[V0_10_MANIFEST_DESIGN.md](V0_10_MANIFEST_DESIGN.md)、[V0_10_RELEASE_PROVENANCE.md](V0_10_RELEASE_PROVENANCE.md) |
| 核心仓库状态 | 开始本计划前，本地 `main` 与 `origin/main` 同步且工作区干净；当前没有 `v0.11` 标签 | `git status --short --branch`、`git ls-remote origin` |
| 桌面消费者 | `AlterSendmer v0.6.0` 已通过 crates.io 使用 `sendmer = "0.10.0"` 并完成正式发布 | [AlterSendmer 主线计划](../../alter-sendme-gpui/docs/MAINLINE.md)、该仓库 `Cargo.toml`、GitHub Release `v0.6.0` |

### 2.2 阶段结论（已完成）

本阶段已按“正式包升级和桌面验收”完成，未向核心端追加协议功能：

1. `AlterSendmer` 已从 `0.9.0` 升级到正式发布的 `0.10.0`，只使用公开接口。
2. 会话过期、接收方上限、主动撤销和 TM1 结果已映射为清晰的桌面状态；桌面端不解释 wire schema。
3. 跨进程回归、Windows 原生窗口/文件系统验收和 Windows/Ubuntu/macOS CI 均已通过。
4. 未发现需要 `sendmer 0.10.x` 修复批次的核心缺陷，也未因桌面需求启动 `v0.11`。

### 2.3 执行结果与发布证据

| 批次 | 当前结果 | 证据 |
| --- | --- | --- |
| P11 | 依赖迁移完成 | AlterSendmer `0d1761a`，`cargo tree -i sendmer@0.10.0` |
| P12 | 会话控制与能力映射完成 | AlterSendmer `ff88953`，状态机/结构化错误测试 |
| P13 | TM1 选择与跨进程兼容完成 | AlterSendmer `4a3e91d`，legacy/TM1 adapter fixture |
| P14 | 原生与跨平台验收完成 | Windows 截图产物、CI run `32859241361` |
| P15 | 版本、文档和正式发布完成 | AlterSendmer tag/Release `v0.6.0`，Release workflow `32866559790`，23 个资产；三个平台打包、签名、SBOM、provenance、更新清单和 checksum 均已上传 |

## 3. 阶段目标与完成定义

### 3.1 阶段目标

交付一个可以发布的 `AlterSendmer` 次版本（建议 `v0.6.0`），它使用 crates.io 的
`sendmer 0.10.0`，保留 `v0.5.0` 的文件传输、缓存和本地化行为，并能安全呈现 v0.10 新增的
会话和 manifest 能力。

### 3.2 完成定义

只有同时满足以下条件，才把本阶段标记为完成：

- `AlterSendmer` 的依赖解析结果来自 crates.io 的 `sendmer 0.10.0`，不存在本地 path、Git revision 或提交哈希替代。
- 适配器只调用公开类型和方法；缓存目录、manifest JSON、Router、provider request 和底层连接 ID 不进入桌面代码。
- 旧 Collection V0 默认保持可发送、可接收；未设置的 sender 上限仍表示不限制；未设置的会话生命周期仍由用户主动停止。
- 用户主动停止、发送会话过期、普通失败和接收取消在 UI、事件和历史中可区分，且不会让旧任务覆盖新一代会话状态。
- 事件信封的 `schema_version`、`session_id`、`sequence`、终态和结构化错误均有适配器测试；未知字段或乱序事件安全失败，不猜测含义。
- legacy 文件、TM1 空目录/元数据、接收缓存恢复、主动撤销和接收方上限均有跨进程或原生文件系统证据。
- Windows 默认窗口、最小窗口、高 DPI、长 ticket、失败重试、设置页和语言浮层完成真实截图验收；Linux/macOS 至少有对应原生 CI 结果。
- 发布说明、版本矩阵、README 和两仓库主线文档同步，发布资产演练通过；未验证的 relay/弱网结果单独标记，不写成已完成。

## 4. 开发批次与出口条件

批次按以下顺序执行。每个批次独立提交；上一批次的出口条件满足后才进入下一批次。

### P11：基线冻结与依赖迁移

**负责仓库：** `F:\project\alter-sendme-gpui`<br>
**前置条件：** sendmer `v0.10.0` tag、crates.io 包和 `main` 文档已可解析。

**工作内容：**

- 将 `Cargo.toml` 的 `sendmer` 依赖改为 `0.10.0`，刷新 `Cargo.lock`，保留 registry source。
- 检查 [transfer.rs](../../alter-sendme-gpui/src/transfer.rs) 的 `SendOptions`、`ReceiveOptions`、事件发射器和取消通道映射；必要时只调整公开字段名和导入路径。
- 将诊断摘要中的 `sendmer=0.9` 更新为实际解析版本，并同步桌面 README 与主线版本矩阵。
- 建立一个最小的 API 检查清单，列出当前使用的 `SendHandle`、`ReceiveCacheOptions`、`ReceiveRetryPolicy`、`TransferEvent` 和 `TransferError`，防止后续误用私有实现。

**出口证据：**

- `cargo metadata --locked` 和 `cargo tree -i sendmer` 显示唯一依赖为 crates.io `sendmer 0.10.0`。
- 桌面端 `fmt`、`check`、Clippy 和测试通过；既有文件发送、接收、取消、缓存和事件测试没有回归。
- 依赖升级单独提交，提交标题使用 `chore: upgrade sendmer dependency to 0.10.0` 或同等单一范围的 Conventional Commit。

### P12：会话控制与能力映射

**负责仓库：** `F:\project\alter-sendme-gpui`；核心端只在发现缺陷时进入独立修复批次。

**工作内容：**

- 显示发送会话的 `SenderTransferStatus::Expired`，并把过期产生的不可重试 `Timeout` 终态与用户主动 `cancel` 区分开。
- 将主动停止继续映射到 `SendHandle::cancel`；验证取消会使旧 ticket 失效，且不会影响已经结束的接收历史。
- 在高级发送设置中加入接收方数量上限，默认保持“不限制”；达到上限时展示远端拒绝或连接失败的稳定摘要，不根据底层文本猜测原因。
- 建议为会话生命周期提供 `15 分钟 / 1 小时 / 8 小时 / 不限时` 预设，默认“不限时”。预设只是桌面输入层，实际值仍转换为 `SendOptions::session_lifetime`，不实现第二个计时器。
- `max_files`、`max_total_size_bytes` 和 `max_import_memory_bytes` 在首个桌面版本中继续保留为 CLI/API 能力，不在没有 UX 和使用场景证据时堆入设置页；后续若暴露，另开独立批次。

**出口证据：**

- 发送、主动停止、过期和重新发送各有状态机测试，旧 generation 的事件不能覆盖新会话。
- 过期后 receiver 使用原 ticket 必须得到失败结果；用户主动停止和过期都完成资源清理且不留下 sender 临时目录。
- 新增文案至少覆盖 English 和简体中文，并通过现有全部 locale key 完整性检查。

### P13：TM1、事件和历史兼容

**负责仓库：** `F:\project\alter-sendme-gpui`。

**工作内容：**

- 发送端提供明确的 manifest 选择；接收端只展示 sendmer 返回的文件名、完成结果和安全错误，不读取或重新实现 TM1 JSON。
- 增加 legacy Collection V0 与 TM1 的适配器 fixture：普通文件、空目录、受支持的权限/修改时间，以及目标平台无法还原的路径拒绝。
- 继续按 `schema_version = 1`、同一 `session_id` 和严格递增 `sequence` 接收事件；未知 schema、重复序号和序号缺口进入安全失败状态。
- 用 `TransferErrorCode` 和 `TransferPhase` 驱动本地化摘要与重试按钮；不把 `message` 当作程序分支条件。
- 保持旧历史 JSON 可读，新字段使用兼容默认值；缓存仍通过 `ReceiveCacheOptions` 和 `prune_receive_cache` 访问，桌面不解析缓存 `manifest.json`。

**出口证据：**

- 两种 collection 模式均完成跨进程 round-trip，失败时 staging 不提交到最终目录。
- 事件序号、终态互斥、错误码和历史迁移各有自动化测试；至少保留一个真实 Windows 文件属性/时间戳检查。
- 适配器 fixture 不包含完整 ticket、绝对路径、节点密钥或底层连接标识。

### P14：跨平台与原生验收

**负责仓库：** 两个仓库；原生截图主要在 `F:\project\alter-sendme-gpui` 执行。

**自动化检查：**

```text
# sendmer 基线或核心修复批次
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo check --workspace --all-features --bins
cargo test --locked --workspace --all-features --bins --tests --examples

# AlterSendmer 依赖升级和桌面批次
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
```

**原生检查：**

- 在 Windows 真实窗口执行 `.scripts\capture-ui-acceptance.ps1 -OutputDirectory <evidence-dir>`，覆盖发送、接收、设置页、语言浮层、默认窗口和 `760x560` 最小窗口。
- 额外操作长 ticket、失败重试、会话过期、接收方上限、manifest 空目录和缓存 prune；截图与操作日志放在桌面仓库的验收产物目录。
- 通过 AlterSendmer 的 Windows、Ubuntu 和 macOS CI；跨平台文件名、权限和时间戳结果以对应 runner 为准。
- relay-only 与弱网重启测试仍需显式设置 `SENDMER_RELAY_SMOKE=1` 或 `SENDMER_WEAK_NETWORK_SMOKE=1`；没有可达 relay 时只能报告“未执行”，不能报告“已验证”。

**出口证据：** 自动化命令、原生截图、CI run 链接、失败场景日志和剩余限制都能按批次追溯；仅编译通过不能替代原生验收。

### P15：版本、文档与发布

**负责仓库：** 两个仓库，先桌面后发布；sendmer 不因桌面适配重新打 `v0.11`。

**工作内容：**

- 桌面端完成版本号、README、更新清单和 release notes；若 P12/P13 的用户能力全部交付，建议发布 `AlterSendmer v0.6.0`。
- 回写两个仓库的主线版本矩阵：sendmer 记录桌面端已消费 `0.10.0`，AlterSendmer 记录正式依赖已升级；不复制稳定契约正文。
- 运行 portable/installer/release manifest 演练，确认资产、checksum、签名和 `latest.json` 与版本号一致。
- 生成 release body 时使用 tag 提交范围，并保留双语章节、非空章节和 `Full Changelog` 链接；重跑同一 tag 必须幂等更新。

**出口证据：**

- 两仓库工作区干净，版本提交、tag、GitHub Release、crates.io 依赖解析和发布资产均可回读。
- 发布顺序固定为：sendmer 契约/修复（如有）→ 发布 crates.io 和 GitHub Release → AlterSendmer 依赖升级与三平台验收 → 发布 AlterSendmer。
- 每个独立批次都在测试通过后提交，并立即推送到其配置的远程；失败或未完成批次不创建 tag。

## 5. 验收矩阵

| 场景 | 预期结果 | 必须保留的证据 |
| --- | --- | --- |
| legacy Collection V0 普通文件 | 发送、接收和 no-replace 行为与 `v0.9.0` 一致 | 桌面 adapter E2E、核心回归测试 |
| TM1 空目录/元数据 | 目录和受支持属性在 staging 全部校验后一次性导出；冲突时无半导出 | Windows 文件系统测试、跨进程 fixture |
| 会话过期 | sender 发布 `Expired`，事件为不可重试 `Timeout`，旧 ticket 无法继续接收 | 状态机测试、原生操作日志 |
| 主动撤销 | `SendHandle::cancel` 完成有序关闭，旧 ticket 失效，接收方得到取消/连接失败摘要 | sender/receiver E2E、清理断言 |
| 接收方数量上限 | 已占用名额不被超额连接绕过，断开后名额释放 | 并发连接 fixture、稳定错误摘要 |
| 事件乱序或未知 schema | 桌面端安全失败，不猜测字段含义，不把旧任务写入新 generation | adapter unit tests |
| 持久接收缓存 | 失败/取消可恢复，成功导出后清理；未知或未来 schema 保留 | cache integration tests、prune report |
| 长 ticket、最小窗口、高 DPI | 输入、布局和错误状态不遮挡；截图来自真实窗口 | Windows `capture-ui-acceptance.ps1` 产物 |
| 发布资产 | archive、checksum、SBOM、签名和 provenance 成组且可验证 | portable/release rehearsal、CI run |

## 6. 风险、决策和回退路径

- **crates.io 解析漂移：** 依赖升级必须检查 `Cargo.lock` 的 source 和 `cargo tree`；任何 path/Git 替代都拒绝进入提交。
- **桌面误解核心语义：** 适配器只消费公开状态、事件和错误；manifest、缓存和限速规则由 sendmer 负责，UI 只展示选择和结果。
- **过期与用户停止混淆：** 以 `SenderTransferStatus` 和结构化事件作为状态来源，不以 ticket 是否还能连接或错误文案猜测。
- **跨平台文件语义差异：** 真实 Windows/Linux/macOS 文件系统验收优先于 JSON fixture；本机未覆盖的平台以 CI 结果为准并明确剩余限制。
- **relay/弱网不可达：** 保留 opt-in 测试和环境前置条件；缺少外部服务时不伪造成功证据。
- **适配发现核心缺陷：** 停止桌面批次，先在 sendmer 建立最小复现和独立 `fix:` 提交；需要发布时走 `0.10.1` 修复版，重新完成核心发布门槛后再恢复桌面适配。
- **范围膨胀：** daemon、跨设备同步、账号、云端存储、多租户控制面、每 peer 配额、接收端下载限速和符号链接支持均不属于本阶段。

## 7. 执行规则与后续检查点

### 7.1 启动顺序

下一次开发从 `F:\project\alter-sendme-gpui` 的 P11 开始：先建立独立分支，升级 registry 依赖，运行桌面四项质量检查，再进入状态和 UI 工作。sendmer 当前 `main` 保持只读基线，除非出现可复现核心缺陷。

### 7.2 提交边界

- P11、P12、P13、P14、P15 分别形成单一范围的 Conventional Commit 或少量同一批次提交，不把依赖升级、UI 重构和发布资产混在一个提交中。
- 每个功能先运行风险匹配的测试；测试失败、原生证据缺失或只完成一部分时，不提交为完成状态。
- 提交成功后立即推送当前分支；远程、签名、部署或原生验收无法完成时，在批次记录中写明阻塞项和替代证据。

### 7.3 文档回写

P11 完成后更新 AlterSendmer 的依赖版本；P15 发布后已同步 [MAINLINE.md](MAINLINE.md) 与
[AlterSendmer 主线计划](../../alter-sendme-gpui/docs/MAINLINE.md) 的版本矩阵和下一主线状态。
本文件在本阶段完成后保留为执行记录，不取代稳定契约、迁移指南或历史 Release Notes。

## 8. 明确延期项

以下事项在没有新的产品边界、认证、持久状态和运维方案前不进入 `sendmer` 下一版本：

- 后台 daemon、跨设备同步、账号/多租户、云端文件托管和自建 relay 运维。
- 需要新的 backpressure 设计的接收端下载限速，以及每个 receiver 独立的上传配额或运行中动态调速。
- 符号链接传输、ACL/owner/security descriptor 等超出 TM1 v1 能力边界的文件系统语义。
- 通过桌面端私自扩展 ticket、缓存布局、Router 或协议字段来绕过核心发布顺序。
