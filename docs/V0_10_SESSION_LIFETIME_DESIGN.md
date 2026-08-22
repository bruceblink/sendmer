# sendmer v0.10 Sender Session Lifetime

## Terminology and Naming

| 规范名 | English / Acronym | 本方案中的职责 | 不代表什么 |
| --- | --- | --- | --- |
| 发送会话生命周期 | Sender Session Lifetime | 从 sender 返回可用 Ticket 后开始计时的固定最长存活时间 | 不是接收端下载空闲超时，也不是 ticket 的永久有效期 |
| 生命周期过期 | Session Expiry | 到达生命周期上限后撤销 router 接受能力并发出终态事件 | 不是网络错误、远端拒绝或资源配额错误 |
| 过期状态 | Expired Status | `SenderTransferStatus::Expired`，供 CLI/GUI 发现需要执行最终清理 | 不是 `Completed` 或 `Aborted` 的别名 |
| 最终清理 | Final Cleanup | `SendHandle::close/cancel` 释放 router、store 和临时目录 | 不是后台任务可绕过 owner 的文件删除 |

## Contract

`SendOptions::session_lifetime` is an optional fixed `Duration`. `None` keeps
the current unlimited behavior. When configured, the countdown starts only
after sender setup has produced a ready `SendResult`; setup failures do not
consume the lifetime.

The expiry task performs these operations in order:

1. Wake provider throttle waits through the existing shutdown signal.
2. Shut down the cloned router, preventing new requests and closing active
   protocol handlers through iroh's normal shutdown path.
3. Emit one non-retryable `timeout` failure at `finalizing` phase, then publish
   `SenderTransferStatus::Expired` so CLI/GUI cleanup cannot race the terminal event.

The `SendResult` owner remains responsible for final resource cleanup. A
subsequent `SendHandle::close` or `cancel` is safe and idempotent: router
shutdown is already complete, and the existing temporary-directory cleanup
removes the sender store. The expiry task is aborted when the owner is dropped
or explicitly closes the share.

The lifetime is deliberately a fixed upper bound, not an idle timer. Extending
it based on transfer activity would make a forgotten ticket live indefinitely
and would require a separate policy for active receivers. Existing callers and
tickets remain unchanged unless they opt in.

## Compatibility and CLI Mapping

The CLI exposes `--session-lifetime-seconds <seconds>` as an optional non-zero
value and maps it to `Duration::from_secs`. The library field remains a
`Duration` so embedders can use sub-second tests and do not depend on CLI units.
The default is absent, preserving the current Ctrl+C-driven sender lifetime.

Expiry is a sender-side lifecycle event only. Receivers observe a normal
connection interruption and may classify it as retryable or terminal according
to their existing retry policy; no new wire schema or ticket format is added.

## Safety and Acceptance

- zero lifetimes are rejected before endpoint setup;
- expiry wakes throttled requests and does not leave a pacing task sleeping;
- only one expiry task exists per sender and it cannot emit a second terminal
  event after an explicit close/cancel;
- the owner can still remove the temporary store after expiry;
- legacy `SendOptions` callers with `None` retain current behavior;
- focused tests cover option validation, status/event ordering, router closure,
  and post-expiry cleanup.
