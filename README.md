# ly-plc

通用 PLC 通讯库（Rust），与具体业务无关，可被任意上位机工程复用。

- **协议**：Modbus TCP、西门子 S7（S7comm / ISO-on-TCP，S7-200 SMART / 300 / 400 / 1200 / 1500）、三菱 MC（3E 帧二进制，Q / L / iQ-R / iQ-F）、内置模拟器
- **通用地址表**：各协议原生地址字符串 + 数据类型 + 字节序 + 读写权限
- **轮询引擎**：按区域合并连续地址批量读取、断线自动重连、心跳、边沿事件
- **日志**：SQLite 存储连接/值变化/写入/异常/边沿等事件，支持条件查询与点位值回放
- **无框架依赖**：通过 `EventSink` 回调向外推送事件，可对接 Tauri、Web 服务或 CLI

## 地址格式

| 协议 | 示例 | 说明 |
| --- | --- | --- |
| Modbus TCP | `HR100` `IR0` `C10` `DI3` `HR5.3` `40001` `30001` | HR/IR/C/DI 为 0 起始偏移；5/6 位传统地址为 1 起始；`.n` 为寄存器位 |
| S7 | `DB1.DBW0` `DB1.DBX2.3` `DB1.8` `M10.0` `MW20` `I0.1` `QD4` `VW100` | 字节寻址，Bool 需带位号；`V` 等价于 `DB1`（200 SMART） |
| MC | `D100` `D100.F` `M10` `X1F` `Y20` `W1A` `R200` `ZR1000` `SM400` | X/Y/B/W/SB/SW 为十六进制（iQ-F 可切换 X/Y 八进制）；字软元件位号为十六进制 |

数据类型：`bool` `u8` `i8` `u16` `i16` `u32` `i32` `f32` `f64` `string` `bytes`（8 位类型仅字节寻址的 S7 可用）。

`string` 仅 S7 可用，对应 S7 `STRING[n]`，地址写成 `起始字节.最大长度`，如 `DB1.DBB38.24` 占 38–63 共 26 字节（2 字节头 + 24 字符），值为 JSON 字符串。读取时 PLC 声明的最大长度必须与点位一致，否则报错；写入只接受单字节字符，超长直接拒绝，同时写最大长度头并以 0 补齐。

`bytes` 仅 S7 可用，是一段原始字节，地址写成 `起始字节.字节数`（1–256），如 `DB1.DBB38.26` 占 38–63。读到的值是 `PlcValue::Bytes`（JSON 为数字数组），不做任何解释；写入需给出恰好等长的数组。需要比 `string` 更宽松的 STRING 解析（容忍声明长度不符、保留原始字节）时，用 `bytes` 读整段区域再自行解析。

字节序：`ABCD`（大端）`CDAB`（字交换）`BADC`（字节交换）`DCBA`（小端）。未指定时 Modbus / S7 默认 `ABCD`，MC 默认 `CDAB`（低字在前）。

## S7 连接注意

- S7-1200/1500 需在 TIA Portal 中启用「允许来自远程对象的 PUT/GET 通信访问」，且被访问的 DB 需取消「优化的块访问」
- 远端 TSAP 由 `连接类型 << 8 | 机架 << 5 | 插槽` 计算，也可直接指定 `localTsap` / `remoteTsap`（如 200 SMART 常用 0x0102 / 0x0201）

## 使用

```rust
use std::sync::Arc;
use ly_plc::{EventSink, Logbook, PlcConfig, PlcEngine, PlcEvent};

let sink: EventSink = Arc::new(|event| match event {
    PlcEvent::Values(changed) => { /* 推送到界面 */ }
    PlcEvent::Edge(edge) => { /* 业务：按 tags 判断是否触发检测 */ }
    _ => {}
});
let logbook = Arc::new(Logbook::open("plc.db".as_ref(), sink.clone(), 30)?);
let engine = PlcEngine::new(PlcConfig::load("plc.json".as_ref()).unwrap_or_default(), logbook, sink);
engine.connect().await;
engine.write_point("result", &serde_json::json!(1)).await?; // Result<(), PlcError>
```

点位的 `tags` 由业务层自由定义，库本身不解释其含义；`edge` 配置后会产生 `PlcEvent::Edge`，携带 tags 供业务分发。

### 按需读与写后确认

轮询引擎每个周期把全部点位按区合并读一遍进缓存，`values()` 取的是缓存。需要「现在」的值时：

```rust
let v = engine.read_now("result").await?;                       // 只读这个点位的区域，不经过也不更新缓存
let report = engine
    .write_point_verified("result", &json!(2), VerifyOptions { attempts: 12, delay: Duration::from_millis(5) })
    .await?;                                                    // 写 → 等 delay → read_now → 比较，不一致就重写
```

二者与轮询、`write_point` 共用同一把驱动锁，串行执行。`write_point_verified` 的期望值按读取的解码路径算（`f32` 舍入一致）；写入 / 读回本身失败立即返回，用尽次数仍不一致返回 `PlcError::Mismatch`，`VerifyError.report` 里有尝试次数与最后一次读回的值。

### 只写、不轮询的点位

点位配置 `"poll": false`（默认 `true`）后不参与轮询：不出现在 `values()` 里、不产生读取错误日志，写入照常，也可以 `read_now`。适合 PC 写给 PLC 的镜像区、应答字节。不轮询的点位不能配置 `edge`。

### 错误

`write_point` / `read_now` / `PointValue.error` / `PlcStatus.last_error` 都是 `PlcError`：

| 变体 | 含义 | `is_link()` |
| --- | --- | --- |
| `Offline` | 没有可用连接（未启动、重连中、已中断；离线标记的点位） | 是 |
| `Link` | 通讯错误（连接被重置、应答帧异常） | 是 |
| `Timeout` | 通讯 / 连接超时 | 是 |
| `Rejected` | PLC 应答了但拒绝（S7 访问失败、COTP 拒绝、Modbus 异常码、MC 结束码） | 否 |
| `Invalid` | 点位不存在、不可写、值无法编码 | 否 |
| `Decode` | 读到的数据无法按类型解码（STRING 头不符等） | 否 |
| `Mismatch` | 写后读回不一致且已用尽次数 | 否 |

`read_now` / 写入遇到链路类错误时会立即关掉当前连接（超时后迟到的应答会错位到下一次请求上），由轮询任务按掉线处理并重连。`PlcError` 序列化为 `{"kind": "timeout", "message": "通讯超时"}`；`PointValue` 的 JSON 仍是 `error` 文本，另加 `errorKind`。`impl From<PlcError> for String`，返回 `Result<_, String>` 的调用方可以继续用 `?`。

### 重拨统计

`PlcStatus` 里：`dials`（`connect()` 以来的拨号总数）、`outage_dials`（本次掉线以来的拨号次数，含成功的那次；链路再次中断时清零，所以恢复后读到的就是这次恢复用了几次）、`last_dial_error`（最近一次拨号失败的原因，连接成功后保留）、`last_error`（最近一次进入 `Error` 状态的原因）。`connect()` 时全部清零。

## 调试工具

```bash
cargo run --example probe -- config.json 点位ID=值 ...
```

读取 JSON 配置、连接 PLC、执行写入并打印全部点位当前值与日志。

无硬件时可用 `tools/` 下的 Modbus / S7 / MC 测试服务端配合 probe 自测，见 [tools/README.md](tools/README.md)。

## 测试

```bash
cargo test                       # 单测 + 对着进程内 S7 测试服务端（tests/support）的集成测试
LY_PLC_S7_PYTHON=<装了 python-snap7 的 python> cargo test --test e2e_snap7 -- --ignored --nocapture
```

`tests/support` 是只实现驱动所需报文的 S7 服务端，能记录每次读请求、拒绝 COTP、只收不回、掐断连接、写入后改回原值；`e2e_snap7` 对着 `tools/s7_server.py` 跑同样的能力。
