# 变更记录

## 0.3.0

### 新增

- `DataType::SmartString`（`"smartString"`，仅 S7）：S7-200 SMART 字符串，首字节为实际长度，没有声明长度字节；地址 `起始字节.最大长度`，区域为最大长度 + 1 字节。与 `string` 仅凭字节无法区分，选错可能静默误读，见 README。

### 行为变化

- S7 点位区域超过配置 `pduSize` 的单帧容量（读 PDU − 18、可写点位 PDU − 28 字节）时 `validate()` 拒绝；协商出的 PDU 更小时，超出的点位读写返回 `Rejected`，不发请求。此前会照常下发，由 PLC 拒绝或断开。

### 破坏性变更

- `DataType` 新增 `SmartString` 变体：穷尽匹配需要补分支。

## 0.2.0

### 新增

- `PlcEngine::read_now(id)`：立即读一个点位（只读它自己的区域，不经过也不更新轮询缓存），与轮询共用驱动锁。
- `PlcEngine::write_point_verified(id, value, VerifyOptions { attempts, delay })`：写 → 等 `delay` → 立即读回 → 比较，不一致重写。返回 `VerifiedWrite { attempts, written, readback }`，失败为 `VerifyError { error, report }`。
- `PlcPoint.poll`（默认 `true`）：为 `false` 时不参与轮询，不进 `values()`、不产生读取错误日志，写入与 `read_now` 照常；不能配 `edge`。
- `DataType::Bytes`（`"bytes"`，仅 S7）：地址 `起始字节.字节数`，值 `PlcValue::Bytes(Vec<u8>)`（JSON 数字数组）。
- `PlcError { Offline, Link, Timeout, Rejected, Invalid, Decode, Mismatch }`（`#[non_exhaustive]`），`is_link()` / `kind()` / `message()`。
- `PlcStatus.dials` / `outage_dials` / `last_dial_error` / `last_error`。
- `DriverError::Timeout`；`PointValue` 的 JSON 增加 `errorKind`。

### 行为变化

- `read_now` / `write_point` 遇到链路类错误（断开、超时、应答帧异常）时立即关闭当前连接，由轮询任务按掉线处理并重连。0.1 会继续使用这条连接，超时后迟到的应答可能被当成下一次请求的应答。

### 破坏性变更

- `PlcEngine::write_point` 返回 `Result<(), PlcError>`（原 `Result<(), String>`）。在返回 `Result<_, String>` 的函数里用 `?` 仍可编译（`impl From<PlcError> for String`）；直接比较 / 返回 `String` 的地方需要 `.map_err(String::from)`。
- `PointValue.error` 是 `Option<PlcError>`（原 `Option<String>`）：`error.as_deref()` 改为 `error.as_ref().map(PlcError::message)`；构造时用具体变体，如 `Some(PlcError::Rejected("..".into()))`。JSON 里 `error` 仍是文本。
- `PlcValue` 新增 `Bytes` 变体；`DataType` 新增 `Bytes` 变体：穷尽匹配需要补分支。
- `PlcPoint` 新增 `poll` 字段、`PlcStatus` 新增四个字段：完整的结构体字面量需要补字段（用 `..Default::default()` 的不受影响）。
- `DriverError` 新增 `Timeout` 变体；`protocol::connect` 与各驱动的 `connect` 返回 `Result<_, DriverError>`（原 `String`）。
