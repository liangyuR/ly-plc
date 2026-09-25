use std::io;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use super::{locate, normalize, split_bit, tcp_connect};
use super::{AreaKey, DriverError, Location, Protocol, Region, RegionData, Unit, WriteValue};
use crate::model::{ConnectionConfig, DataType};

pub const AREA_I: u16 = 0x81;
pub const AREA_Q: u16 = 0x82;
pub const AREA_M: u16 = 0x83;
pub const AREA_DB: u16 = 0x84;

fn strip_size(s: &str) -> &str {
    match s.as_bytes().first() {
        Some(b'X' | b'B' | b'W' | b'D') => &s[1..],
        _ => s,
    }
}

pub fn resolve(address: &str, data_type: DataType) -> Result<Location, String> {
    let a = normalize(address);
    let err = || {
        format!("无法识别的 S7 地址「{address}」，示例：DB1.DBW0、DB1.DBX2.3、DB1.8、M10.0、MW20、I0.1、QD4、VW100")
    };
    let (code, db, rest) = if let Some(r) = a.strip_prefix("DB") {
        let (db, rest) = r.split_once('.').ok_or_else(err)?;
        let db: u16 = db.parse().map_err(|_| err())?;
        if db == 0 {
            return Err("DB 编号从 1 开始".into());
        }
        let rest = rest.strip_prefix("DB").map(strip_size).unwrap_or(rest);
        (AREA_DB, db, rest)
    } else {
        let (code, db) = match a.as_bytes().first() {
            Some(b'I' | b'E') => (AREA_I, 0),
            Some(b'Q' | b'A') => (AREA_Q, 0),
            Some(b'M') => (AREA_M, 0),
            Some(b'V') => (AREA_DB, 1),
            _ => return Err(err()),
        };
        (code, db, strip_size(&a[1..]))
    };
    let (main, bit) = split_bit(rest, 10)?;
    let offset: u32 = main.parse().map_err(|_| err())?;
    locate(
        AreaKey {
            code,
            index: db,
            unit: Unit::Byte,
        },
        offset,
        bit,
        data_type,
        true,
        0xFFFF,
    )
}

fn item_error(code: u8) -> String {
    let text = match code {
        0x01 => "硬件故障",
        0x03 => "访问被拒绝（S7-1200/1500 需关闭 DB 优化块访问并启用 PUT/GET）",
        0x05 => "地址越界",
        0x06 => "数据类型不支持",
        0x07 => "数据长度不一致",
        0x0A => "对象不存在（DB 未定义或长度不足）",
        _ => "未知错误",
    };
    format!("S7 访问失败 0x{code:02X}：{text}")
}

fn header_error(s7: &[u8]) -> Option<String> {
    if s7.len() < 12 || s7[1] != 0x03 {
        return None;
    }
    let (class, code) = (s7[10], s7[11]);
    if class == 0 && code == 0 {
        return None;
    }
    let hint = if class == 0x81 || class == 0x85 {
        "（请确认 CPU 已启用 PUT/GET 访问）"
    } else {
        ""
    };
    Some(format!(
        "S7 请求被拒绝 class=0x{class:02X} code=0x{code:02X}{hint}"
    ))
}

pub struct S7 {
    stream: TcpStream,
    timeout: Duration,
    pdu: u16,
    seq: u16,
}

impl S7 {
    pub async fn connect(cfg: &ConnectionConfig) -> Result<Self, String> {
        let dur = Duration::from_millis(cfg.timeout_ms);
        let stream = tcp_connect(&cfg.host, cfg.port, dur).await?;
        let mut client = Self {
            stream,
            timeout: dur,
            pdu: cfg.s7.pdu_size,
            seq: 0,
        };
        let (local, remote) = cfg.s7.tsaps();
        let [lh, ll] = local.to_be_bytes();
        let [rh, rl] = remote.to_be_bytes();
        let cr = [
            0x03, 0x00, 0x00, 0x16, 0x11, 0xE0, 0x00, 0x00, 0x00, 0x01, 0x00, 0xC0, 0x01, 0x0A,
            0xC1, 0x02, lh, ll, 0xC2, 0x02, rh, rl,
        ];
        let cc = client
            .roundtrip(&cr)
            .await
            .map_err(|e| format!("COTP 连接失败: {e}"))?;
        if cc.get(5) != Some(&0xD0) {
            return Err(format!(
                "COTP 连接被拒绝，请检查机架/插槽或 TSAP (远端 0x{remote:04X})"
            ));
        }
        let [ph, pl] = cfg.s7.pdu_size.to_be_bytes();
        let setup = [
            0x32, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0xF0, 0x00, 0x00, 0x01,
            0x00, 0x01, ph, pl,
        ];
        let s7 = client
            .exchange(&setup)
            .await
            .map_err(|e| format!("S7 通讯协商失败: {e}"))?;
        if let Some(e) = header_error(&s7) {
            return Err(e);
        }
        if s7.len() >= 20 {
            client.pdu = u16::from_be_bytes([s7[18], s7[19]]).max(240);
        }
        Ok(client)
    }

    async fn roundtrip(&mut self, frame: &[u8]) -> Result<Vec<u8>, DriverError> {
        let stream = &mut self.stream;
        let io = async {
            stream.write_all(frame).await?;
            let mut head = [0u8; 4];
            stream.read_exact(&mut head).await?;
            let len = u16::from_be_bytes([head[2], head[3]]) as usize;
            if head[0] != 0x03 || len < 7 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "TPKT 帧头异常"));
            }
            let mut buf = vec![0u8; len];
            buf[..4].copy_from_slice(&head);
            stream.read_exact(&mut buf[4..]).await?;
            Ok(buf)
        };
        timeout(self.timeout, io)
            .await
            .map_err(|_| DriverError::Link("通讯超时".into()))?
            .map_err(|e| DriverError::Link(format!("通讯错误: {e}")))
    }

    async fn exchange(&mut self, payload: &[u8]) -> Result<Vec<u8>, DriverError> {
        self.seq = self.seq.wrapping_add(1);
        let mut frame = Vec::with_capacity(payload.len() + 7);
        let len = (payload.len() + 7) as u16;
        frame.extend_from_slice(&[0x03, 0x00]);
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(&[0x02, 0xF0, 0x80]);
        frame.extend_from_slice(payload);
        frame[11..13].copy_from_slice(&self.seq.to_be_bytes());
        let resp = self.roundtrip(&frame).await?;
        if resp.len() < 7 + 12 || resp[5] != 0xF0 || resp[7] != 0x32 {
            return Err(DriverError::Link("S7 响应格式异常".into()));
        }
        Ok(resp[7..].to_vec())
    }

    fn item_spec(transport: u8, len: u16, loc_db: u16, area: u8, bit_addr: u32) -> [u8; 12] {
        let [lh, ll] = len.to_be_bytes();
        let [dh, dl] = loc_db.to_be_bytes();
        [
            0x12,
            0x0A,
            0x10,
            transport,
            lh,
            ll,
            dh,
            dl,
            area,
            (bit_addr >> 16) as u8,
            (bit_addr >> 8) as u8,
            bit_addr as u8,
        ]
    }
}

#[async_trait]
impl Protocol for S7 {
    async fn read(&mut self, region: &Region) -> Result<RegionData, DriverError> {
        let count = region.count as u16;
        let mut req = vec![
            0x32, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0E, 0x00, 0x00, 0x04, 0x01,
        ];
        req.extend_from_slice(&Self::item_spec(
            0x02,
            count,
            region.area.index,
            region.area.code as u8,
            region.start * 8,
        ));
        let s7 = self.exchange(&req).await?;
        if let Some(e) = header_error(&s7) {
            return Err(DriverError::Exception(e));
        }
        if s7.len() < 18 {
            return Err(DriverError::Link("S7 读响应长度异常".into()));
        }
        if s7[14] != 0xFF {
            return Err(DriverError::Exception(item_error(s7[14])));
        }
        let mut len = u16::from_be_bytes([s7[16], s7[17]]) as usize;
        if matches!(s7[15], 0x03..=0x05) {
            len /= 8;
        }
        let data = s7
            .get(18..18 + len)
            .ok_or_else(|| DriverError::Link("S7 读数据长度异常".into()))?;
        if data.len() != count as usize {
            return Err(DriverError::Exception(format!(
                "S7 返回 {} 字节，期望 {count}",
                data.len()
            )));
        }
        Ok(RegionData::Bytes(data.to_vec()))
    }

    async fn write(&mut self, loc: &Location, value: &WriteValue) -> Result<(), DriverError> {
        let (spec, ts, bits, data) = match value {
            WriteValue::Bit(b) => {
                let addr = loc.start * 8 + loc.bit.unwrap_or(0) as u32;
                (
                    Self::item_spec(0x01, 1, loc.area.index, loc.area.code as u8, addr),
                    0x03,
                    1u16,
                    vec![*b as u8],
                )
            }
            WriteValue::Bytes(bytes) => {
                let n = bytes.len() as u16;
                (
                    Self::item_spec(0x02, n, loc.area.index, loc.area.code as u8, loc.start * 8),
                    0x04,
                    n * 8,
                    bytes.clone(),
                )
            }
        };
        let dlen = (4 + data.len()) as u16;
        let mut req = vec![0x32, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0E];
        req.extend_from_slice(&dlen.to_be_bytes());
        req.extend_from_slice(&[0x05, 0x01]);
        req.extend_from_slice(&spec);
        req.extend_from_slice(&[0x00, ts]);
        req.extend_from_slice(&bits.to_be_bytes());
        req.extend_from_slice(&data);
        let s7 = self.exchange(&req).await?;
        if let Some(e) = header_error(&s7) {
            return Err(DriverError::Exception(e));
        }
        match s7.get(14) {
            Some(0xFF) => Ok(()),
            Some(code) => Err(DriverError::Exception(item_error(*code))),
            None => Err(DriverError::Link("S7 写响应长度异常".into())),
        }
    }

    fn max_region(&self, _unit: Unit) -> (u32, u32) {
        ((self.pdu as u32).saturating_sub(18).max(32), 16)
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
        let cases = [
            ("DB1.DBW10", DataType::I16, AREA_DB, 1, 10, None),
            ("db12.dbx3.7", DataType::Bool, AREA_DB, 12, 3, Some(7)),
            ("DB5.20", DataType::F32, AREA_DB, 5, 20, None),
            ("MW20", DataType::U16, AREA_M, 0, 20, None),
            ("M10.2", DataType::Bool, AREA_M, 0, 10, Some(2)),
            ("I0.1", DataType::Bool, AREA_I, 0, 0, Some(1)),
            ("QD4", DataType::F32, AREA_Q, 0, 4, None),
            ("VW100", DataType::I16, AREA_DB, 1, 100, None),
            ("MB3", DataType::U8, AREA_M, 0, 3, None),
        ];
        for (addr, dt, code, db, start, bit) in cases {
            let l = resolve(addr, dt).unwrap_or_else(|e| panic!("{addr}: {e}"));
            assert_eq!(
                (l.area.code, l.area.index, l.start, l.bit),
                (code, db, start, bit),
                "{addr}"
            );
        }
        assert_eq!(resolve("DB1.DBD0", DataType::F64).unwrap().len, 8);
        assert!(resolve("M10.8", DataType::Bool).is_err());
        assert!(resolve("M10", DataType::Bool).is_err());
    }
}
