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

数据类型：`bool` `u8` `i8` `u16` `i16` `u32` `i32` `f32` `f64`（8 位类型仅字节寻址的 S7 可用）。

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
engine.write_point("result", &serde_json::json!(1)).await?;
```

点位的 `tags` 由业务层自由定义，库本身不解释其含义；`edge` 配置后会产生 `PlcEvent::Edge`，携带 tags 供业务分发。

## 调试工具

```bash
cargo run --example probe -- config.json 点位ID=值 ...
```

读取 JSON 配置、连接 PLC、执行写入并打印全部点位当前值与日志。

无硬件时可用 `tools/` 下的 Modbus / S7 / MC 测试服务端配合 probe 自测，见 [tools/README.md](tools/README.md)。
