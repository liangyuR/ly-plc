use std::time::Duration;

use async_trait::async_trait;
use tokio::time::{error::Elapsed, timeout};
use tokio_modbus::client::{Client, Context, Reader, Writer};
use tokio_modbus::Slave;

use super::{bytes_to_words, locate, normalize, split_bit, words_to_bytes};
use super::{AreaKey, DriverError, Location, Protocol, Region, RegionData, Unit, WriteValue};
use crate::model::{ConnectionConfig, DataType};

pub const COIL: u16 = 0;
pub const DISCRETE_INPUT: u16 = 1;
pub const INPUT_REGISTER: u16 = 3;
pub const HOLDING_REGISTER: u16 = 4;

pub fn area(code: u16) -> AreaKey {
    let unit = if code == COIL || code == DISCRETE_INPUT {
        Unit::Bit
    } else {
        Unit::Word
    };
    AreaKey {
        code,
        index: 0,
        unit,
    }
}

pub fn resolve(address: &str, data_type: DataType) -> Result<Location, String> {
    let a = normalize(address);
    let err =
        || format!("无法识别的 Modbus 地址「{address}」，示例：HR100、IR0、C10、DI3、HR5.3、40001");
    let (main, bit) = split_bit(&a, 10)?;
    const PREFIXES: [(&str, u16); 5] = [
        ("COIL", COIL),
        ("HR", HOLDING_REGISTER),
        ("IR", INPUT_REGISTER),
        ("DI", DISCRETE_INPUT),
        ("C", COIL),
    ];
    let (code, offset) = if let Some((p, code)) = PREFIXES.iter().find(|(p, _)| main.starts_with(p))
    {
        (*code, main[p.len()..].parse::<u32>().map_err(|_| err())?)
    } else if main.chars().all(|c| c.is_ascii_digit()) && (main.len() == 5 || main.len() == 6) {
        let code = match main.as_bytes()[0] {
            b'0' => COIL,
            b'1' => DISCRETE_INPUT,
            b'3' => INPUT_REGISTER,
            b'4' => HOLDING_REGISTER,
            _ => return Err(err()),
        };
        let n: u32 = main[1..].parse().map_err(|_| err())?;
        if n == 0 {
            return Err("传统地址从 1 开始，例如 40001".into());
        }
        (code, n - 1)
    } else {
        return Err(err());
    };
    let writable = code == COIL || code == HOLDING_REGISTER;
    locate(area(code), offset, bit, data_type, writable, 0xFFFF)
}

pub struct ModbusTcp {
    ctx: Context,
    timeout: Duration,
}

fn flatten<T>(r: Result<tokio_modbus::Result<T>, Elapsed>) -> Result<T, DriverError> {
    match r {
        Err(_) => Err(DriverError::Timeout("通讯超时".into())),
        Ok(Err(e)) => Err(DriverError::Link(format!("通讯错误: {e}"))),
        Ok(Ok(Err(code))) => Err(DriverError::Exception(format!("PLC 异常响应: {code:?}"))),
        Ok(Ok(Ok(v))) => Ok(v),
    }
}

impl ModbusTcp {
    pub async fn connect(cfg: &ConnectionConfig) -> Result<Self, DriverError> {
        let dur = Duration::from_millis(cfg.timeout_ms);
        let addr = super::resolve_addr(&cfg.host, cfg.port, dur).await?;
        let ctx = timeout(
            dur,
            tokio_modbus::client::tcp::connect_slave(addr, Slave(cfg.modbus.unit_id)),
        )
        .await
        .map_err(|_| DriverError::Timeout(format!("连接超时: {addr}")))?
        .map_err(|e| DriverError::Link(format!("连接失败 {addr}: {e}")))?;
        Ok(Self { ctx, timeout: dur })
    }

    async fn read_words(
        &mut self,
        code: u16,
        addr: u16,
        cnt: u16,
    ) -> Result<Vec<u16>, DriverError> {
        let t = self.timeout;
        let r = if code == HOLDING_REGISTER {
            timeout(t, self.ctx.read_holding_registers(addr, cnt)).await
        } else {
            timeout(t, self.ctx.read_input_registers(addr, cnt)).await
        };
        flatten(r)
    }
}

#[async_trait]
impl Protocol for ModbusTcp {
    async fn read(&mut self, region: &Region) -> Result<RegionData, DriverError> {
        let (addr, cnt, t) = (region.start as u16, region.count as u16, self.timeout);
        match region.area.code {
            COIL => flatten(timeout(t, self.ctx.read_coils(addr, cnt)).await).map(RegionData::Bits),
            DISCRETE_INPUT => flatten(timeout(t, self.ctx.read_discrete_inputs(addr, cnt)).await)
                .map(RegionData::Bits),
            code => self
                .read_words(code, addr, cnt)
                .await
                .map(|w| RegionData::Bytes(words_to_bytes(&w))),
        }
    }

    async fn write(&mut self, loc: &Location, value: &WriteValue) -> Result<(), DriverError> {
        let (addr, t) = (loc.start as u16, self.timeout);
        match (loc.area.code, value) {
            (COIL, WriteValue::Bit(b)) => {
                flatten(timeout(t, self.ctx.write_single_coil(addr, *b)).await)
            }
            (HOLDING_REGISTER, WriteValue::Bit(b)) => {
                let bit = loc.bit.unwrap_or(0);
                let current = self.read_words(HOLDING_REGISTER, addr, 1).await?[0];
                let next = if *b {
                    current | (1 << bit)
                } else {
                    current & !(1 << bit)
                };
                flatten(timeout(t, self.ctx.write_single_register(addr, next)).await)
            }
            (HOLDING_REGISTER, WriteValue::Bytes(bytes)) => {
                let words = bytes_to_words(bytes);
                let r = if words.len() == 1 {
                    timeout(t, self.ctx.write_single_register(addr, words[0])).await
                } else {
                    timeout(t, self.ctx.write_multiple_registers(addr, &words)).await
                };
                flatten(r)
            }
            _ => Err(DriverError::Exception("该区域不可写".into())),
        }
    }

    fn max_region(&self, unit: Unit) -> (u32, u32) {
        if unit == Unit::Bit {
            (1000, 64)
        } else {
            (120, 16)
        }
    }

    async fn close(&mut self) {
        let _ = timeout(self.timeout, self.ctx.disconnect()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_addresses() {
        let l = resolve("hr100", DataType::F32).unwrap();
        assert_eq!((l.area.code, l.start, l.len), (HOLDING_REGISTER, 100, 2));
        let l = resolve("40001", DataType::U16).unwrap();
        assert_eq!((l.area.code, l.start), (HOLDING_REGISTER, 0));
        let l = resolve("HR5.3", DataType::Bool).unwrap();
        assert_eq!(l.bit, Some(3));
        assert!(resolve("DI0", DataType::U16).is_err());
        assert!(resolve("HR0", DataType::String).is_err());
        assert!(!resolve("IR2", DataType::U16).unwrap().writable);
    }
}
