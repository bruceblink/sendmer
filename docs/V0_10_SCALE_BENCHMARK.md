# sendmer v0.10 规模基准

## 术语表

| 规范名称 | English / 缩写 | 当前基准中的职责 | 不代表什么 |
| --- | --- | --- | --- |
| 导入工作集预算 | Import Working-Set Budget | 限制并行 sender 导入任务持有的估算普通文件字节 | 不是进程 RSS、堆分配总量或操作系统硬内存配额 |
| sender setup 耗时 | Sender Setup Time | 从 `send` 开始到返回可用 Ticket 的耗时，包含文件导入和 router 初始化 | 不代表接收端网络吞吐或完整传输耗时 |
| 大文件案例 | Large-File Case | 一个 16 MiB 普通文件的 sender setup | 不代表任意文件大小的线性性能保证 |
| 大目录案例 | Large-Directory Case | 256 个 64 KiB 普通文件的 sender setup，总 payload 也是 16 MiB | 不代表目录元数据或空目录 manifest 已支持 |

## 运行方式

基准目标位于 `benches/share_scale.rs`，使用 release bench profile，并且每次成功
`send` 后立即关闭 sender。默认命令为：

```powershell
cargo bench --locked --bench share_scale
```

可用环境变量：

| 变量 | 默认值 | 含义 |
| --- | ---: | --- |
| `SENDMER_BENCH_ITERATIONS` | `3` | 每个案例的迭代次数 |
| `SENDMER_BENCH_FILE_BYTES` | `16777216` | 大文件案例字节数 |
| `SENDMER_BENCH_DIRECTORY_FILES` | `256` | 大目录案例普通文件数 |
| `SENDMER_BENCH_DIRECTORY_FILE_BYTES` | `65536` | 大目录案例每个文件字节数 |
| `SENDMER_BENCH_IMPORT_MEMORY_BYTES` | unset | 传给 `SendOptions::max_import_memory_bytes` 的预算 |

基准输出每次迭代的 `setup_ms`，以及 `min/avg/max` 汇总；不设置性能通过阈值，避免把
机器负载、文件系统缓存或网络环境抖动误报为失败。

## 本机基线

运行环境：Windows `x86_64-pc-windows-msvc`，Rust `1.97.1`，`import_memory_bytes=None`。
命令：`cargo bench --locked --bench share_scale`。

```text
case=large-file summary=min_ms:63.597,avg_ms:69.338,max_ms:80.454
case=large-directory summary=min_ms:54.551,avg_ms:64.536,max_ms:69.981
```

这些数值是当前机器的参考快照，不是 release gate；改变案例大小或导入预算后应重新记录
完整输出和运行环境。

## Relay Smoke

relay-only 验收不进入默认离线门禁。设置 `SENDMER_RELAY_SMOKE=1` 后，可选用
`SENDMER_RELAY_URL` 指定 relay URL（未设置时使用 iroh production default relay），再运行：

```powershell
$env:SENDMER_RELAY_SMOKE = "1"
cargo test --locked --test relay_smoke -- --ignored --nocapture
```

测试强制 Ticket 只包含 relay 地址，验证 sender online、receiver round-trip 和 payload 完整性。
