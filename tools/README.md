# 测试工具

无 PLC 硬件时，用本地测试服务端配合 `examples/probe` 验证各协议驱动。

## 准备

```bash
python -m venv .venv
.venv/Scripts/pip install -r tools/requirements.txt   # Linux/macOS: .venv/bin/pip
```

## 测试服务端

| 脚本 | 协议 | 默认端口 | 预置数据 |
| --- | --- | --- | --- |
| `modbus_server.py` | Modbus TCP（pymodbus） | 1502 | HR20-21 = 12.5 (F32)，IR5 = 42 |
| `s7_server.py` | S7（python-snap7） | 1102 | DB1.DBW4 = -7，DB1.DBD20 = 12.5，M10.2 = 1；另注册 DB2 |
| `mc_server.py` | 三菱 MC 3E 二进制帧（自带实现） | 5007 | D100-101 = 12.5 (F32，低字在前)，X1F = 1 |

参数：`--port` 改端口；`s7_server.py --dump` 每秒打印内存；`mc_server.py -v` 打印每条请求。

## 验证

先启动对应服务端，再运行 probe（`点位ID=值` 表示写入后回读）：

```bash
python tools/modbus_server.py
cargo run --example probe -- tools/configs/modbus.json hr20=2.5 cdab=305419896 bit=true

python tools/s7_server.py --dump
cargo run --example probe -- tools/configs/s7.json i16=1234 f32=3.75 f64=-2.5e10 u8=200 mw12=65000

python tools/mc_server.py -v
cargo run --example probe -- tools/configs/mc.json d100=2.5 d102=-100000 d120a=true m3=true
```

预期要点：

- Modbus：`HR20` 写 2.5 后寄存器为 `0x4020 0x0000`；`40031` (CDAB) 写 0x12345678 后为 `0x5678 0x1234`；`IR5` 只读
- S7：`DB9.DBW0` 返回「对象不存在」；8/16/32/64 位读写回读一致
- MC：D100 写 2.5 后 D100 = 0x0000、D101 = 0x4020（低字在前）；`D120.A` 写 true 后 D120 = 1024

## 已知限制

- `s7_server.py` 基于 python-snap7 纯 Python 服务端，单 BIT 写入（如 `DB1.DBX0.3`）会忽略位偏移写到 bit 0；驱动按 S7 规范发送位地址，真实 PLC 正常
- `mc_server.py` 依据协议规范实现，仅覆盖批量读写（0401/1401，字/位单位），仅用于驱动自测，最终以真机为准
