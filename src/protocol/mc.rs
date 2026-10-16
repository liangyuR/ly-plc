use std::io;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use super::{locate, normalize, split_bit, tcp_connect};
use super::{AreaKey, DriverError, Location, Protocol, Region, RegionData, Unit, WriteValue};
use crate::model::{ConnectionConfig, DataType, McOptions};

enum Radix {
    Dec,
    Hex,
    Xy,
}

const DEVICES: &[(&str, u8, Unit, Radix)] = &[
    ("ZR", 0xB0, Unit::Word, Radix::Dec),
    ("SM", 0x91, Unit::Bit, Radix::Dec),
    ("SD", 0xA9, Unit::Word, Radix::Dec),
    ("SB", 0xA1, Unit::Bit, Radix::Hex),
    ("SW", 0xB5, Unit::Word, Radix::Hex),
    ("TS", 0xC1, Unit::Bit, Radix::Dec),
    ("TN", 0xC2, Unit::Word, Radix::Dec),
    ("CS", 0xC4, Unit::Bit, Radix::Dec),
    ("CN", 0xC5, Unit::Word, Radix::Dec),
    ("X", 0x9C, Unit::Bit, Radix::Xy),
    ("Y", 0x9D, Unit::Bit, Radix::Xy),
    ("M", 0x90, Unit::Bit, Radix::Dec),
    ("L", 0x92, Unit::Bit, Radix::Dec),
    ("F", 0x93, Unit::Bit, Radix::Dec),
    ("V", 0x94, Unit::Bit, Radix::Dec),
    ("B", 0xA0, Unit::Bit, Radix::Hex),
    ("D", 0xA8, Unit::Word, Radix::Dec),
    ("W", 0xB4, Unit::Word, Radix::Hex),
    ("R", 0xAF, Unit::Word, Radix::Dec),
    ("Z", 0xCC, Unit::Word, Radix::Dec),
];

pub fn resolve(address: &str, data_type: DataType, xy_octal: bool) -> Result<Location, String> {
    let a = normalize(address);
    let err = || {
        format!(
            "无法识别的 MC 地址「{address}」，示例：D100、D100.F、M10、X1F、Y20、W1A、R200、ZR1000"
        )
    };
    let (name, code, unit, radix) = DEVICES
        .iter()
        .find(|(p, ..)| a.starts_with(p))
        .ok_or_else(err)?;
    let (main, bit) = split_bit(&a[name.len()..], 16)?;
    let radix = match radix {
        Radix::Dec => 10,
        Radix::Hex => 16,
        Radix::Xy if xy_octal => 8,
        Radix::Xy => 16,
    };
    let number = u32::from_str_radix(main, radix).map_err(|_| err())?;
    locate(
        AreaKey {
            code: *code as u16,
            index: 0,
            unit: *unit,
        },
        number,
        bit,
        data_type,
        true,
        0xFF_FFFF,
    )
}

pub struct Mc {
    stream: TcpStream,
    timeout: Duration,
    opts: McOptions,
    timer: u16,
}

impl Mc {
    pub async fn connect(cfg: &ConnectionConfig) -> Result<Self, DriverError> {
        let dur = Duration::from_millis(cfg.timeout_ms);
        let stream = tcp_connect(&cfg.host, cfg.port, dur).await?;
        let timer = (cfg.timeout_ms / 250).clamp(1, 0xFFFF) as u16;
        Ok(Self {
            stream,
            timeout: dur,
            opts: cfg.mc.clone(),
            timer,
        })
    }

    fn frame(&self, command: u16, sub: u16, body: &[u8]) -> Vec<u8> {
        let len = (6 + body.len()) as u16;
        let o = &self.opts;
        let mut f = vec![0x50, 0x00, o.network_no, o.pc_no];
        f.extend_from_slice(&o.module_io.to_le_bytes());
        f.push(o.module_station);
        f.extend_from_slice(&len.to_le_bytes());
        f.extend_from_slice(&self.timer.to_le_bytes());
        f.extend_from_slice(&command.to_le_bytes());
        f.extend_from_slice(&sub.to_le_bytes());
        f.extend_from_slice(body);
        f
    }

    async fn request(
        &mut self,
        command: u16,
        sub: u16,
        body: &[u8],
    ) -> Result<Vec<u8>, DriverError> {
        let frame = self.frame(command, sub, body);
        let stream = &mut self.stream;
        let io = async {
            stream.write_all(&frame).await?;
            let mut head = [0u8; 9];
            stream.read_exact(&mut head).await?;
            if head[0] != 0xD0 || head[1] != 0x00 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "MC 响应帧头异常",
                ));
            }
            let len = u16::from_le_bytes([head[7], head[8]]) as usize;
            if len < 2 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "MC 响应长度异常",
                ));
            }
            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf).await?;
            Ok(buf)
        };
        let buf = timeout(self.timeout, io)
            .await
            .map_err(|_| DriverError::Timeout("通讯超时".into()))?
            .map_err(|e| DriverError::Link(format!("通讯错误: {e}")))?;
        let end = u16::from_le_bytes([buf[0], buf[1]]);
        if end != 0 {
            return Err(DriverError::Exception(format!(
                "PLC 异常结束码 0x{end:04X}"
            )));
        }
        Ok(buf[2..].to_vec())
    }

    fn device_body(code: u16, start: u32, count: u16) -> Vec<u8> {
        let s = start.to_le_bytes();
        let c = count.to_le_bytes();
        vec![s[0], s[1], s[2], code as u8, c[0], c[1]]
    }

    async fn read_words(
        &mut self,
        code: u16,
        start: u32,
        count: u16,
    ) -> Result<Vec<u8>, DriverError> {
        let data = self
            .request(0x0401, 0x0000, &Self::device_body(code, start, count))
            .await?;
        if data.len() != count as usize * 2 {
            return Err(DriverError::Link("MC 字读取长度异常".into()));
        }
        Ok(data.chunks(2).flat_map(|w| [w[1], w[0]]).collect())
    }

    async fn write_words(
        &mut self,
        code: u16,
        start: u32,
        be_bytes: &[u8],
    ) -> Result<(), DriverError> {
        let mut body = Self::device_body(code, start, (be_bytes.len() / 2) as u16);
        body.extend(be_bytes.chunks(2).flat_map(|w| [w[1], w[0]]));
        self.request(0x1401, 0x0000, &body).await.map(|_| ())
    }
}

#[async_trait]
impl Protocol for Mc {
    async fn read(&mut self, region: &Region) -> Result<RegionData, DriverError> {
        let (code, count) = (region.area.code, region.count as u16);
        if region.area.unit == Unit::Word {
            return self
                .read_words(code, region.start, count)
                .await
                .map(RegionData::Bytes);
        }
        let data = self
            .request(
                0x0401,
                0x0001,
                &Self::device_body(code, region.start, count),
            )
            .await?;
        if data.len() != (count as usize).div_ceil(2) {
            return Err(DriverError::Link("MC 位读取长度异常".into()));
        }
        let bits = (0..count as usize)
            .map(|i| {
                let byte = data[i / 2];
                if i % 2 == 0 {
                    byte >> 4 != 0
                } else {
                    byte & 0x0F != 0
                }
            })
            .collect();
        Ok(RegionData::Bits(bits))
    }

    async fn write(&mut self, loc: &Location, value: &WriteValue) -> Result<(), DriverError> {
        let code = loc.area.code;
        match (loc.area.unit, value) {
            (Unit::Bit, WriteValue::Bit(b)) => {
                let mut body = Self::device_body(code, loc.start, 1);
                body.push(if *b { 0x10 } else { 0x00 });
                self.request(0x1401, 0x0001, &body).await.map(|_| ())
            }
            (Unit::Word, WriteValue::Bit(b)) => {
                let bit = loc.bit.unwrap_or(0);
                let cur = self.read_words(code, loc.start, 1).await?;
                let w = u16::from_be_bytes([cur[0], cur[1]]);
                let next = if *b { w | (1 << bit) } else { w & !(1 << bit) };
                self.write_words(code, loc.start, &next.to_be_bytes()).await
            }
            (Unit::Word, WriteValue::Bytes(bytes)) => {
                self.write_words(code, loc.start, bytes).await
            }
            _ => Err(DriverError::Exception("数据类型与软元件不匹配".into())),
        }
    }

    fn max_region(&self, unit: Unit) -> (u32, u32) {
        if unit == Unit::Bit {
            (3584, 128)
        } else {
            (960, 32)
        }
    }

    async fn close(&mut self) {
        let _ = timeout(self.timeout, self.stream.shutdown()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_addresses() {
        let l = resolve("D100", DataType::I32, false).unwrap();
        assert_eq!((l.area.code, l.start, l.len), (0xA8, 100, 2));
        let l = resolve("d100.f", DataType::Bool, false).unwrap();
        assert_eq!(l.bit, Some(15));
        assert_eq!(resolve("X1F", DataType::Bool, false).unwrap().start, 0x1F);
        assert_eq!(resolve("X17", DataType::Bool, true).unwrap().start, 15);
        assert_eq!(
            resolve("ZR1000", DataType::U16, false).unwrap().area.code,
            0xB0
        );
        assert_eq!(
            resolve("SM400", DataType::Bool, false).unwrap().area.code,
            0x91
        );
        assert!(resolve("M10", DataType::U16, false).is_err());
        assert!(resolve("X18", DataType::Bool, true).is_err());
    }
}
